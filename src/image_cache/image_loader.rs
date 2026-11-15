use std::borrow::Cow;
use std::collections::VecDeque;
use std::fs;
use std::io::{BufReader, Read};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, Sender, TryRecvError, channel};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;

use gelatin::image::{
	self, AnimationDecoder, ImageDecoder, ImageFormat,
	codecs::{gif::GifDecoder, png::PngDecoder},
};
use usvg::fontdb;

#[cfg(target_os = "macos")]
#[path = "system_decoder.rs"]
pub mod system_decoder;

#[derive(Debug, thiserror::Error)]
#[error("error happened in the image_loader: {description}")]
pub struct ImageLoaderError {
	pub description: Cow<'static, str>,
}
impl From<std::io::Error> for ImageLoaderError {
	fn from(value: std::io::Error) -> Self {
		ImageLoaderError { description: format!("IO error: {value}").into() }
	}
}
impl From<image::ImageError> for ImageLoaderError {
	fn from(value: image::ImageError) -> Self {
		ImageLoaderError { description: format!("image-crate error: {value}").into() }
	}
}
impl From<usvg::Error> for ImageLoaderError {
	fn from(value: usvg::Error) -> Self {
		ImageLoaderError { description: format!("usvg error: {value}").into() }
	}
}

pub type Result<T> = std::result::Result<T, ImageLoaderError>;

/// Priority id meaning "no request is prioritised": the loader threads take any request.
/// See `LoaderShared::priority`.
pub const NON_EXISTENT_REQUEST_ID: u32 = u32::MAX;

/// The gamut texture pixels are encoded in. Both use the sRGB transfer curve, so the textures
/// stay `SrgbTexture2d`s and only the primaries differ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Primaries {
	Srgb,
	DisplayP3,
}

/// The gamut of the window pictures are shown in. macOS windows are Display P3 (the OS matches
/// that to the actual display); everywhere else the window is sRGB.
pub const WINDOW_PRIMARIES: Primaries =
	if cfg!(target_os = "macos") { Primaries::DisplayP3 } else { Primaries::Srgb };

/// Linear sRGB to linear Display P3 (both D65), as columns for GLSL's `mat3`.
#[rustfmt::skip]
const SRGB_TO_DISPLAY_P3: [[f32; 3]; 3] = [
	[0.822_461_97, 0.033_194_2, 0.017_082_63],
	[0.177_538_03, 0.966_805_8, 0.072_397_44],
	[0.0, 0.0, 0.910_519_93],
];

impl Primaries {
	/// The `mat3` that takes this gamut's linear colour to the window's in the fragment shader.
	/// Only sRGB pictures in a wider window need one: wider pictures are converted on load.
	pub fn to_window_matrix(self) -> [[f32; 3]; 3] {
		match (self, WINDOW_PRIMARIES) {
			(Primaries::Srgb, Primaries::DisplayP3) => SRGB_TO_DISPLAY_P3,
			_ => [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
		}
	}

	/// An sRGB-gamut colour constant of the UI (the linear values the colour shaders write to
	/// the sRGB framebuffer) in the window's gamut. Greys are unchanged.
	pub fn window_color(color: [f32; 4]) -> [f32; 4] {
		let m = Primaries::Srgb.to_window_matrix();
		let [r, g, b, a] = color;
		let channel = |row: usize| m[0][row] * r + m[1][row] * g + m[2][row] * b;
		[channel(0), channel(1), channel(2), a]
	}

	fn profile(self) -> moxcms::ColorProfile {
		match self {
			Primaries::Srgb => moxcms::ColorProfile::new_srgb(),
			Primaries::DisplayP3 => moxcms::ColorProfile::new_display_p3(),
		}
	}
}

/// Largest side of the box SVGs are first rendered into, whatever the window size.
pub const SVG_INITIAL_MAX_SIDE: u32 = 4096;

/// Where nothing is known about the window: the size every SVG was once rendered at.
pub const SVG_DEFAULT_FIT: (u32, u32) = (SVG_INITIAL_MAX_SIDE, SVG_INITIAL_MAX_SIDE);

/// Largest side of an SVG render: the texture grid tiles anything up to this.
const SVG_MAX_SIDE: f32 = 16384.0;
/// Largest area of an SVG render (256 MB of RGBA pixels, plus a copy on the GPU).
const SVG_MAX_PIXELS: f32 = 64.0 * 1024.0 * 1024.0;

pub enum ImgFormat {
	Image(ImageFormat),
	Svg,
	/// HEIC/HEIF (and AVIF without our own decoder), decoded by the OS.
	#[cfg(target_os = "macos")]
	System,
	/// Camera RAW, shown through its embedded preview by the OS decoder.
	#[cfg(target_os = "macos")]
	Raw,
}

/// Camera RAW formats. Most are TIFF containers, so the magic bytes alone would make them look
/// like (undecodable) TIFFs; only the OS decoder on macOS can read them.
const RAW_EXTENSIONS: [&str; 10] =
	["cr2", "cr3", "nef", "arw", "dng", "raf", "orf", "rw2", "pef", "srw"];

fn has_extension(path: &Path, extensions: &[&str]) -> bool {
	path.extension().is_some_and(|ext| extensions.iter().any(|e| ext.eq_ignore_ascii_case(e)))
}

/// These values define the transformation for a pixel array which is to be displayed.
///
/// The default case is when the 0th row is at the top and the 0th column is at the left side of the
/// image. This is represented by the value `Deg0`. All other cases must be interpreted as relative
/// to this. The rotation part is counter-clockwise. When there's a flip it's always interpreted as
/// if it happened after the rotation.
#[derive(Default, Debug, Copy, Clone)]
pub enum Orientation {
	/// Exif 1
	#[default]
	Deg0,

	/// Exif 2
	Deg0HorFlip,

	/// Exif 3
	Deg180,

	/// Exif 4
	Deg180HorFlip,

	/// Exif 5
	Deg90VerFlip,

	/// Exif 6
	Deg270,

	/// Exif 7
	Deg270VerFlip,

	/// Exif 8
	Deg90,
}

/// Whether the bytes are an ISO base media file of a HEIF flavour (`ftyp` box with a HEIC brand).
#[cfg(target_os = "macos")]
fn is_heif_magic(bytes: &[u8]) -> bool {
	bytes
		.get(4..12)
		.is_some_and(|ftyp| matches!(ftyp, b"ftypheic" | b"ftypheix" | b"ftypmif1" | b"ftyphevc"))
}

/// The formats only the macOS decoder handles, if the file is one of them.
#[cfg(target_os = "macos")]
fn detect_system_format(path: &Path, file_start: Option<&[u8]>) -> Option<ImgFormat> {
	if has_extension(path, &RAW_EXTENSIONS) {
		return Some(ImgFormat::Raw);
	}
	if has_extension(path, &["heic", "heif", "hif"]) || file_start.is_some_and(is_heif_magic) {
		return Some(ImgFormat::System);
	}
	#[cfg(not(feature = "avif"))]
	if file_start.is_some_and(|bytes| matches!(image::guess_format(bytes), Ok(ImageFormat::Avif))) {
		return Some(ImgFormat::System);
	}
	None
}

/// Detects the format of an image file. It looks at the first 512 bytes;
/// if that fails, it uses the file ending.
pub fn detect_format(path: &Path) -> Result<ImgFormat> {
	let mut file = fs::File::open(path)?;
	if path.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("svg")) {
		return Ok(ImgFormat::Svg);
	}
	let mut file_start_bytes = [0; 512];
	let file_start =
		file.read_exact(&mut file_start_bytes).is_ok().then_some(&file_start_bytes[..]);

	#[cfg(target_os = "macos")]
	if let Some(format) = detect_system_format(path, file_start) {
		return Ok(format);
	}

	// Try to detect the format from the first 512 bytes
	if let Some(format) = file_start.and_then(|bytes| image::guess_format(bytes).ok()) {
		return Ok(ImgFormat::Image(format));
	}

	// If that didn't work, try to detect the format from the file ending
	Ok(ImgFormat::Image(ImageFormat::from_path(path)?))
}

pub fn detect_orientation(path: &Path) -> std::result::Result<Orientation, exif::Error> {
	let file = std::fs::File::open(path)?;
	let mut bufreader = std::io::BufReader::new(&file);
	let exifreader = exif::Reader::new();
	let exif = exifreader.read_from_container(&mut bufreader)?;
	if let Some(orientation) = exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY) {
		if let exif::Value::Short(ref shorts) = orientation.value {
			if let Some(&exif_orientation) = shorts.first() {
				// According to page 30 of http://www.cipa.jp/std/documents/e/DC-008-2012_E.pdf
				match exif_orientation {
					1 => Ok(Orientation::Deg0),
					2 => Ok(Orientation::Deg0HorFlip),
					3 => Ok(Orientation::Deg180),
					4 => Ok(Orientation::Deg180HorFlip),
					5 => Ok(Orientation::Deg90VerFlip),
					6 => Ok(Orientation::Deg270),
					7 => Ok(Orientation::Deg270VerFlip),
					8 => Ok(Orientation::Deg90),
					_ => {
						eprintln!("Invalid Exif orientation. Using default orientation.");
						Ok(Orientation::Deg0)
					}
				}
			} else {
				Ok(Orientation::Deg0)
			}
		} else {
			Err("EXIF orientation was expected to be of type 'short' but it wasn't".into())
		}
	} else {
		Ok(Orientation::Deg0)
	}
}

/// Decodes `path` with the `image` crate. The pixels come back in the window's gamut or in sRGB
/// (see `Primaries`).
pub fn simple_load_image(
	path: &Path,
	image_format: ImageFormat,
) -> Result<(image::RgbaImage, Primaries)> {
	let reader = BufReader::new(fs::File::open(path)?);
	let mut decoder = image::ImageReader::with_format(reader, image_format).into_decoder()?;
	let icc = decoder.icc_profile().ok().flatten();
	let image = image::DynamicImage::from_decoder(decoder)?;
	Ok(match (image, icc) {
		// Radiance HDR and OpenEXR are linear light and can exceed 1.0.
		(
			image @ (image::DynamicImage::ImageRgb32F(_) | image::DynamicImage::ImageRgba32F(_)),
			_,
		) => (tone_map_to_rgba8(&image.into_rgba32f()), Primaries::Srgb),
		// Convert at 16 bits so the profile transform doesn't compound 8-bit rounding.
		(
			image @ (image::DynamicImage::ImageRgb16(_) | image::DynamicImage::ImageRgba16(_)),
			Some(icc),
		) => {
			let mut image = image.into_rgba16();
			let primaries = convert_to_window_space_16(&mut image, &icc);
			let (width, height) = image.dimensions();
			let bytes = image.into_raw().into_iter().map(|v| ((v as u32 + 128) / 257) as u8);
			let image = image::RgbaImage::from_raw(width, height, bytes.collect())
				.expect("buffer length is unchanged");
			(image, primaries)
		}
		(image, icc) => {
			let mut image = image.into_rgba8();
			let primaries = match icc {
				Some(icc) => convert_to_window_space(&mut image, &icc),
				None => Primaries::Srgb,
			};
			(image, primaries)
		}
	})
}

/// Maps linear float pixels to sRGB. When the image has values above 1.0 the luminance is
/// compressed with the extended Reinhard operator (white point = the brightest pixel), which
/// keeps the hue; otherwise the values are only gamma encoded.
fn tone_map_to_rgba8(image: &image::Rgba32FImage) -> image::RgbaImage {
	let luminance = |p: &[f32; 4]| 0.2126 * p[0] + 0.7152 * p[1] + 0.0722 * p[2];
	let max_luminance =
		image.pixels().map(|p| luminance(&p.0)).filter(|l| l.is_finite()).fold(0.0, f32::max);
	let white_sq = max_luminance * max_luminance;
	let tone_map = max_luminance > 1.0;
	let encode = |linear: f32| {
		let linear = linear.clamp(0.0, 1.0);
		let encoded = if linear <= 0.003_130_8 {
			12.92 * linear
		} else {
			1.055 * linear.powf(1.0 / 2.4) - 0.055
		};
		(encoded * 255.0 + 0.5) as u8
	};
	let mut out = Vec::with_capacity(image.as_raw().len());
	for pixel in image.pixels() {
		let [r, g, b, a] = pixel.0;
		let l = luminance(&pixel.0);
		let scale = if tone_map && l.is_finite() && l > 0.0 {
			l * (1.0 + l / white_sq) / (1.0 + l) / l
		} else {
			1.0
		};
		// A NaN channel propagates through `encode` and the saturating `as u8` turns it into 0.
		out.extend_from_slice(&[encode(r * scale), encode(g * scale), encode(b * scale)]);
		out.push((a.clamp(0.0, 1.0) * 255.0 + 0.5) as u8);
	}
	image::RgbaImage::from_raw(image.width(), image.height(), out).expect("four bytes per pixel")
}

/// The gamut the pixels of an image tagged with `icc` have once shown in the window, and the
/// profiles to convert them with if that takes a conversion. sRGB, non-RGB and unparsable
/// profiles take none: the pixels are taken as sRGB. Neither does Display P3 in a Display P3
/// window.
fn profile_conversion(
	icc: &[u8],
) -> (Primaries, Option<(moxcms::ColorProfile, moxcms::ColorProfile)>) {
	use moxcms::{ColorProfile, DataColorSpace, ToneReprCurve};
	let Ok(profile) = ColorProfile::new_from_slice(icc) else {
		return (Primaries::Srgb, None);
	};
	if profile.color_space != DataColorSpace::Rgb {
		return (Primaries::Srgb, None);
	}
	let close = |a: moxcms::Xyzd, b: moxcms::Xyzd| {
		(a.x - b.x).abs() < 1e-3 && (a.y - b.y).abs() < 1e-3 && (a.z - b.z).abs() < 1e-3
	};
	// Primaries and tone curves both: P3 primaries with gamma 2.6 (DCI-P3) or sRGB primaries with
	// gamma 2.2 are not the sRGB / Display P3 the window and shaders assume.
	let is_like = |reference: Primaries| {
		let reference = reference.profile();
		let same_curve = |a: &Option<ToneReprCurve>, b: &Option<ToneReprCurve>| {
			let (Some(Ok(a)), Some(Ok(b))) = (
				a.as_ref().map(ToneReprCurve::make_linear_evaluator),
				b.as_ref().map(ToneReprCurve::make_linear_evaluator),
			) else {
				return false;
			};
			[0.1, 0.25, 0.5, 0.75, 0.9]
				.into_iter()
				.all(|x| (a.evaluate_value(x) - b.evaluate_value(x)).abs() < 1e-3)
		};
		close(profile.red_colorant, reference.red_colorant)
			&& close(profile.green_colorant, reference.green_colorant)
			&& close(profile.blue_colorant, reference.blue_colorant)
			&& same_curve(&profile.red_trc, &reference.red_trc)
			&& same_curve(&profile.green_trc, &reference.green_trc)
			&& same_curve(&profile.blue_trc, &reference.blue_trc)
	};
	if is_like(Primaries::Srgb) {
		return (Primaries::Srgb, None);
	}
	if WINDOW_PRIMARIES == Primaries::DisplayP3 && is_like(Primaries::DisplayP3) {
		return (Primaries::DisplayP3, None);
	}
	(WINDOW_PRIMARIES, Some((profile, WINDOW_PRIMARIES.profile())))
}

/// Re-encodes the pixels from one profile to another in place. `false`: no such transform.
fn recolor(
	image: &mut image::RgbaImage,
	from: &moxcms::ColorProfile,
	to: &moxcms::ColorProfile,
) -> bool {
	use moxcms::{Layout, TransformOptions};
	let Ok(transform) =
		from.create_transform_8bit(Layout::Rgba, to, Layout::Rgba, TransformOptions::default())
	else {
		return false;
	};
	let width = image.width() as usize;
	transform_in_bands(image, width, &*transform);
	true
}

/// Same as `recolor` for 16 bit samples.
fn recolor_16(
	image: &mut image::ImageBuffer<image::Rgba<u16>, Vec<u16>>,
	from: &moxcms::ColorProfile,
	to: &moxcms::ColorProfile,
) -> bool {
	use moxcms::{Layout, TransformOptions};
	let Ok(transform) =
		from.create_transform_16bit(Layout::Rgba, to, Layout::Rgba, TransformOptions::default())
	else {
		return false;
	};
	let width = image.width() as usize;
	transform_in_bands(image, width, &*transform);
	true
}

/// Converts pixels tagged with an RGB ICC profile (Adobe RGB, Display P3...) to the window's
/// gamut, unless that is not needed (see `profile_conversion`). Returns the gamut the pixels
/// are in afterwards.
pub fn convert_to_window_space(image: &mut image::RgbaImage, icc: &[u8]) -> Primaries {
	match profile_conversion(icc) {
		(primaries, None) => primaries,
		(primaries, Some((from, to))) => {
			if recolor(image, &from, &to) {
				primaries
			} else {
				Primaries::Srgb
			}
		}
	}
}

/// Same as `convert_to_window_space` for 16 bit samples.
fn convert_to_window_space_16(
	image: &mut image::ImageBuffer<image::Rgba<u16>, Vec<u16>>,
	icc: &[u8],
) -> Primaries {
	match profile_conversion(icc) {
		(primaries, None) => primaries,
		(primaries, Some((from, to))) => {
			if recolor_16(image, &from, &to) {
				primaries
			} else {
				Primaries::Srgb
			}
		}
	}
}

/// Re-encodes pixels from one gamut to another. Nothing happens when they are the same.
pub fn change_primaries(image: &mut image::RgbaImage, from: Primaries, to: Primaries) {
	if from != to {
		recolor(image, &from.profile(), &to.profile());
	}
}

/// Applies `transform` in place to RGBA `samples` (rows `width` pixels wide).
/// 64-row bands keep each worker's scratch buffer small; the bands are split across cores.
fn transform_in_bands<T: Copy + Default + Send + Sync>(
	samples: &mut [T],
	width: usize,
	transform: &(dyn moxcms::TransformExecutor<T> + Send + Sync),
) {
	let band = (width * 4 * 64).max(4);
	let workers = thread::available_parallelism().map_or(1, |n| n.get());
	let part = band * samples.len().div_ceil(band).div_ceil(workers).max(1);
	thread::scope(|scope| {
		for part in samples.chunks_mut(part) {
			scope.spawn(move || {
				let mut scratch = vec![T::default(); band];
				for chunk in part.chunks_mut(band) {
					let out = &mut scratch[..chunk.len()];
					if transform.transform(chunk, out).is_ok() {
						chunk.copy_from_slice(out);
					}
				}
			});
		}
	});
}

/// Returns an iterator over the animation frames of a GIF file
pub fn load_gif(path: &Path, req_id: u32) -> Result<impl Iterator<Item = Result<LoadResult>>> {
	let file = fs::File::open(path)?;
	let reader = BufReader::new(file);
	let decoder = GifDecoder::new(reader)?;
	Ok(load_animation(req_id, decoder))
}

/// System fonts are scanned once; scanning them per SVG costs more than most renders.
static SVG_FONTS: std::sync::LazyLock<Arc<fontdb::Database>> = std::sync::LazyLock::new(|| {
	let mut fontdb = fontdb::Database::new();
	fontdb.load_system_fonts();
	Arc::new(fontdb)
});

/// Size to render an SVG of `width` x `height` at: the largest that fits in `fit` (physical
/// pixels) without exceeding `SVG_MAX_SIDE` or `SVG_MAX_PIXELS`.
pub fn svg_render_size(width: f32, height: f32, fit: (u32, u32)) -> (u32, u32) {
	let zoom = (fit.0 as f32 / width)
		.min(fit.1 as f32 / height)
		.min(SVG_MAX_SIDE / width.max(height))
		.min((SVG_MAX_PIXELS / (width * height)).sqrt());
	(((width * zoom) as u32).max(1), ((height * zoom) as u32).max(1))
}

/// Parse and render an SVG at the size `svg_render_size` gives for `fit`
pub fn load_svg(path: &Path, fit: (u32, u32)) -> Result<image::RgbaImage> {
	let svg_data = fs::read(path)?;
	let opt = usvg::Options {
		resources_dir: fs::canonicalize(path).ok().and_then(|p| p.parent().map(Path::to_path_buf)),
		fontdb: SVG_FONTS.clone(),
		..Default::default()
	};
	let rtree = usvg::Tree::from_data(&svg_data, &opt)?;
	let (svg_width, svg_height) = (rtree.size().width(), rtree.size().height());
	let (width, height) = svg_render_size(svg_width, svg_height, fit);
	let invalid_size = || ImageLoaderError { description: "invalid SVG size".into() };
	let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or_else(invalid_size)?;
	let transform =
		tiny_skia::Transform::from_scale(width as f32 / svg_width, height as f32 / svg_height);
	resvg::render(&rtree, transform, &mut pixmap.as_mut());
	// tiny-skia renders premultiplied; the textures and shaders expect straight alpha
	image::RgbaImage::from_raw(width, height, pixmap.take_demultiplied()).ok_or_else(invalid_size)
}

/// Decodes the image at `path` and feeds its frames to `process_image`, to be shown in the
/// window. Only the first frame of an animation is decoded unless `allow_animation`. Returns
/// `true` when later frames exist but were not decoded. An SVG is rendered to fit `svg_fit`.
pub fn load_image_frames<F>(
	path: &Path,
	allow_animation: bool,
	req_id: u32,
	svg_fit: (u32, u32),
	mut process_image: F,
) -> Result<bool>
where
	F: FnMut(LoadResult) -> Result<()>,
{
	let image_format = detect_format(path)?;
	let orientation = detect_orientation(path).unwrap_or(Orientation::Deg0);
	let mut truncated = false;

	match image_format {
		ImgFormat::Image(ImageFormat::Gif) => {
			let frames = load_gif(path, req_id)?;
			truncated = process_frames(frames, allow_animation, &mut process_image)?;
		}
		ImgFormat::Image(ImageFormat::Png) => {
			let file = fs::File::open(path)?;
			let reader = BufReader::new(file);
			let decoder = PngDecoder::new(reader)?;
			if decoder.is_apng()? {
				let animation = load_animation(req_id, decoder.apng()?);
				truncated = process_frames(animation, allow_animation, &mut process_image)?;
			} else {
				let (image, primaries) = simple_load_image(path, ImageFormat::Png)?;
				process_image(LoadResult::still(req_id, image, orientation, primaries, false))?;
			}
		}
		ImgFormat::Image(image_format) => {
			let (image, primaries) = simple_load_image(path, image_format)?;
			process_image(LoadResult::still(req_id, image, orientation, primaries, false))?;
		}
		ImgFormat::Svg => {
			let image = load_svg(path, svg_fit)?;
			process_image(LoadResult::still(req_id, image, orientation, Primaries::Srgb, true))?;
		}
		// ImageIO knows the orientation of these better than the EXIF reader (HEIF `irot`).
		#[cfg(target_os = "macos")]
		ImgFormat::System | ImgFormat::Raw => {
			let (image, orientation) =
				system_decoder::load(path, matches!(image_format, ImgFormat::Raw))?;
			let primaries = Primaries::DisplayP3;
			process_image(LoadResult::still(req_id, image, orientation, primaries, false))?;
		}
	}

	Ok(truncated)
}

/// `load_image_frames` for consumers that want plain sRGB pixels (the clipboard) and don't
/// care about the size of SVGs.
pub fn complex_load_image<F>(
	path: &Path,
	allow_animation: bool,
	req_id: u32,
	mut process_image: F,
) -> Result<bool>
where
	F: FnMut(LoadResult) -> Result<()>,
{
	load_image_frames(path, allow_animation, req_id, SVG_DEFAULT_FIT, |mut frame| {
		if let LoadResult::Frame { image, primaries, .. } = &mut frame {
			change_primaries(image, *primaries, Primaries::Srgb);
			*primaries = Primaries::Srgb;
		}
		process_image(frame)
	})
}

/// Hands the frames of an animation to `process_image`, or only the first one unless
/// `all_frames`. Returns whether frames were left out, so a prefetch knows the animation
/// has to be loaded again once it is shown.
fn process_frames(
	mut frames: impl Iterator<Item = Result<LoadResult>>,
	all_frames: bool,
	mut process_image: impl FnMut(LoadResult) -> Result<()>,
) -> Result<bool> {
	if all_frames {
		for frame in frames {
			process_image(frame?)?;
		}
		return Ok(false);
	}
	if let Some(frame) = frames.next() {
		process_image(frame?)?;
	}
	// Decoding one more frame is the only way to tell an animation from a still.
	Ok(frames.next().is_some())
}

fn load_animation(
	req_id: u32,
	decoder: impl AnimationDecoder<'static>,
) -> impl Iterator<Item = Result<LoadResult>> {
	let frames = decoder.into_frames();

	frames.map(move |frame| {
		Ok(frame.map(|frame| {
			let (mut numerator_ms, mut denom) = frame.delay().numer_denom_ms();
			if numerator_ms == 0 {
				// Some animated gifs specify a 0 inter-frame delay, but
				// most gif viewers interpret this as a 0.1 second delay.
				numerator_ms = 100;
				denom = 1;
			}
			let numerator_nano = numerator_ms as u64 * 1_000_000;
			let delay_nano = numerator_nano / (denom as u64);
			let image = frame.into_buffer();
			LoadResult::Frame {
				req_id,
				image,
				delay_nano,
				orientation: Orientation::Deg0,
				primaries: Primaries::Srgb,
				svg: false,
			}
		})?)
	})
}

/// Whether the file extension alone says the file is an image: it doesn't touch the file.
pub fn has_image_extension(filename: &Path) -> bool {
	// Only macOS can decode RAW; elsewhere their TIFF header would list them as broken entries.
	if has_extension(filename, &RAW_EXTENSIONS) {
		return cfg!(target_os = "macos");
	}
	if let Some(ext) = filename.extension()
		&& let Some(ext) = ext.to_str()
	{
		let ext = ext.to_lowercase();
		match ext.as_str() {
			"jpg" | "jpeg" | "png" | "apng" | "gif" | "webp" | "tif" | "tiff" | "tga" | "bmp"
			| "ico" | "hdr" | "pbm" | "pam" | "ppm" | "pgm" => {
				return true;
			}
			#[cfg(target_os = "macos")]
			"heic" | "heif" | "hif" => return true,
			#[cfg(feature = "avif")]
			"avif" => return true,
			_ => (),
		}
	}
	false
}

pub fn is_file_supported(filename: &Path) -> bool {
	if has_image_extension(filename) {
		return true;
	}
	// RAW files are not decodable elsewhere, whatever their content says.
	if has_extension(filename, &RAW_EXTENSIONS) {
		return false;
	}
	detect_format(filename).is_ok()
}

#[derive(Debug, Clone)]
pub struct LoadRequest {
	pub req_id: u32,
	pub path: PathBuf,
	/// Decode every frame of an animation. Prefetches only decode the first one.
	pub all_frames: bool,
	/// The box in physical pixels an SVG is rendered to fit.
	pub svg_fit: (u32, u32),
	/// Set by the cache when nobody wants the result any more; the loader then stops decoding
	/// and sends nothing.
	pub cancelled: Arc<AtomicBool>,
}

impl LoadRequest {
	pub fn new(req_id: u32, path: PathBuf, all_frames: bool) -> Self {
		LoadRequest {
			req_id,
			path,
			all_frames,
			svg_fit: SVG_DEFAULT_FIT,
			cancelled: Arc::new(AtomicBool::new(false)),
		}
	}

	fn is_cancelled(&self) -> bool {
		self.cancelled.load(Ordering::Relaxed)
	}
}

pub enum LoadResult {
	Start {
		req_id: u32,
		metadata: fs::Metadata,
	},
	Frame {
		req_id: u32,
		image: image::RgbaImage,
		delay_nano: u64,

		/// How much does the image need to be rotated counter-clockwise to be shown correctly
		orientation: Orientation,
		/// The gamut the pixels are encoded in.
		primaries: Primaries,
		/// Rendered from an SVG, so it can be rendered again at another size.
		svg: bool,
	},
	Done {
		req_id: u32,
		/// The request decoded only the first frame of an animation.
		truncated: bool,
	},
	Failed {
		req_id: u32,
	},
}

impl LoadResult {
	fn still(
		req_id: u32,
		image: image::RgbaImage,
		orientation: Orientation,
		primaries: Primaries,
		svg: bool,
	) -> Self {
		LoadResult::Frame { req_id, image, delay_nano: 0, orientation, primaries, svg }
	}

	pub fn req_id(&self) -> u32 {
		match self {
			LoadResult::Start { req_id, .. } => *req_id,
			LoadResult::Frame { req_id, .. } => *req_id,
			LoadResult::Done { req_id, .. } => *req_id,
			LoadResult::Failed { req_id, .. } => *req_id,
		}
	}
}

/// State shared between the `ImageLoader` and its worker threads.
struct LoaderShared {
	running: AtomicBool,
	queue: Mutex<VecDeque<LoadRequest>>,
	/// Notified whenever a request is queued, the priority changes or the loader shuts down.
	wake: Condvar,
	/// We want to prevent prefetch operations taking place when the target image is not yet
	/// loaded. Workers only pick the request with this id, or any request if it is
	/// `NON_EXISTENT_REQUEST_ID`. The worker that finishes the priority request clears it.
	priority: AtomicU32,
}

impl LoaderShared {
	fn lock_queue(&self) -> MutexGuard<'_, VecDeque<LoadRequest>> {
		self.queue.lock().unwrap_or_else(PoisonError::into_inner)
	}

	/// Blocks until there is a request a worker may take. Returns `None` on shutdown.
	fn next_request(&self) -> Option<LoadRequest> {
		let mut queue = self.lock_queue();
		loop {
			if !self.running.load(Ordering::Acquire) {
				return None;
			}
			let priority = self.priority.load(Ordering::SeqCst);
			let position = if priority == NON_EXISTENT_REQUEST_ID {
				(!queue.is_empty()).then_some(0)
			} else {
				queue.iter().position(|request| request.req_id == priority)
			};
			if let Some(request) = position.and_then(|position| queue.remove(position)) {
				return Some(request);
			}
			queue = self.wake.wait(queue).unwrap_or_else(PoisonError::into_inner);
		}
	}

	fn set_priority(&self, req_id: u32) {
		self.priority.store(req_id, Ordering::SeqCst);
		self.notify();
	}

	fn clear_priority(&self, req_id: u32) {
		if self
			.priority
			.compare_exchange(req_id, NON_EXISTENT_REQUEST_ID, Ordering::SeqCst, Ordering::SeqCst)
			.is_ok()
		{
			self.notify();
		}
	}

	/// Taking the queue lock first guarantees that a worker which has just evaluated the
	/// (stale) priority is already waiting, so the wake-up cannot be lost.
	fn notify(&self) {
		drop(self.lock_queue());
		self.wake.notify_all();
	}
}

pub struct ImageLoader {
	shared: Arc<LoaderShared>,
	join_handles: Vec<thread::JoinHandle<()>>,
	image_rx: Receiver<LoadResult>,
}

impl ImageLoader {
	pub fn new(threads: u32) -> ImageLoader {
		let shared = Arc::new(LoaderShared {
			running: AtomicBool::new(true),
			queue: Mutex::new(VecDeque::new()),
			wake: Condvar::new(),
			priority: AtomicU32::new(NON_EXISTENT_REQUEST_ID),
		});
		let (loaded_img_tx, loaded_img_rx) = channel();

		let mut join_handles = Vec::new();
		for _ in 0..threads {
			let shared = shared.clone();
			let img_sender = loaded_img_tx.clone();
			join_handles.push(thread::spawn(move || Self::thread_loop(&shared, &img_sender)));
		}

		ImageLoader { shared, join_handles, image_rx: loaded_img_rx }
	}

	fn thread_loop(shared: &LoaderShared, img_sender: &Sender<LoadResult>) {
		while let Some(request) = shared.next_request() {
			let req_id = request.req_id;
			let receiver_alive = Self::load_and_send(img_sender, request);
			shared.clear_priority(req_id);
			if !receiver_alive {
				return;
			}
		}
	}

	pub fn try_recv_prefetched(&mut self) -> std::result::Result<LoadResult, TryRecvError> {
		self.image_rx.try_recv()
	}

	pub fn send_load_request(&mut self, request: LoadRequest) {
		self.shared.lock_queue().push_back(request);
		self.shared.wake.notify_all();
	}

	/// Makes the workers decode `req_id` before anything else.
	pub fn set_priority(&self, req_id: u32) {
		self.shared.set_priority(req_id);
	}

	/// Lifts the priority restriction if it is still `req_id`.
	pub fn clear_priority(&self, req_id: u32) {
		self.shared.clear_priority(req_id);
	}

	/// Returns `false` if the receiving end is gone and the worker should stop.
	fn load_and_send(img_sender: &Sender<LoadResult>, request: LoadRequest) -> bool {
		fn disconnected<E>(_: E) -> ImageLoaderError {
			ImageLoaderError { description: Cow::Borrowed("result receiver disconnected") }
		}
		/// Returns whether the animation was cut short (see `LoadRequest::all_frames`).
		fn try_load_and_send(
			img_sender: &Sender<LoadResult>,
			request: &LoadRequest,
		) -> Result<bool> {
			let metadata = fs::metadata(&request.path)?;
			img_sender
				.send(LoadResult::Start { req_id: request.req_id, metadata })
				.map_err(disconnected)?;
			load_image_frames(
				&request.path,
				request.all_frames,
				request.req_id,
				request.svg_fit,
				|frame| {
					if request.is_cancelled() {
						return Err(ImageLoaderError {
							description: Cow::Borrowed("request cancelled"),
						});
					}
					img_sender.send(frame).map_err(disconnected)
				},
			)
		}

		if request.is_cancelled() {
			return true;
		}
		// A decoder panicking on a malformed file must not kill the worker or leave the
		// request pending forever.
		let outcome =
			panic::catch_unwind(AssertUnwindSafe(|| try_load_and_send(img_sender, &request)));
		// The cache has forgotten a cancelled request: whatever happened is of no interest.
		if request.is_cancelled() {
			return true;
		}
		let result = match outcome {
			Ok(Ok(truncated)) => LoadResult::Done { req_id: request.req_id, truncated },
			Ok(Err(error)) => {
				log::error!(
					"Request #{}: Error occurred while loading file {:?}\n    {}",
					request.req_id,
					request.path,
					error,
				);
				LoadResult::Failed { req_id: request.req_id }
			}
			Err(_) => {
				log::error!(
					"Request #{}: Decoder panicked while loading file {:?}",
					request.req_id,
					request.path,
				);
				LoadResult::Failed { req_id: request.req_id }
			}
		};
		img_sender.send(result).is_ok()
	}
}

impl Drop for ImageLoader {
	fn drop(&mut self) {
		self.shared.running.store(false, Ordering::Release);
		self.shared.notify();
		for handle in self.join_handles.drain(..) {
			if handle.join().is_err() {
				log::error!("A load worker thread panicked");
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// Shorter than the 512 byte sniff window, and half transparent: tiny-skia's premultiplied
	/// output would read back as 50% gray instead of 50% white.
	#[test]
	fn small_translucent_svg_loads_with_straight_alpha() {
		let path = std::env::temp_dir().join(format!("emulsion-test-{}.SVG", std::process::id()));
		fs::write(
			&path,
			r#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="white" fill-opacity="0.5"/></svg>"#,
		)
		.unwrap();
		let format = detect_format(&path);
		let image = load_svg(&path, (8, 8));
		fs::remove_file(&path).unwrap();
		assert!(matches!(format.unwrap(), ImgFormat::Svg));
		let pixel = image.unwrap().get_pixel(4, 4).0;
		assert_eq!(pixel[..3], [255, 255, 255]);
		assert!((127..=128).contains(&pixel[3]));
	}

	/// A Display P3 PNG must end up in the window's gamut: converted to sRGB in an sRGB window,
	/// left alone (and flagged) in a Display P3 one. Shown raw in an sRGB window, the skin tone
	/// would be pushed towards red.
	#[test]
	fn display_p3_png_is_converted_to_window_space() {
		use image::ImageEncoder;
		use moxcms::{ColorProfile, Layout, TransformOptions};
		let skin = [224u8, 172, 140, 255];
		let p3 = ColorProfile::new_display_p3();
		let to_p3 = ColorProfile::new_srgb()
			.create_transform_8bit(Layout::Rgba, &p3, Layout::Rgba, TransformOptions::default())
			.unwrap();
		let mut p3_pixel = [0u8; 4];
		to_p3.transform(&skin, &mut p3_pixel).unwrap();
		assert_ne!(p3_pixel, skin);

		let path =
			std::env::temp_dir().join(format!("emulsion-test-{}-p3.png", std::process::id()));
		let mut encoder = image::codecs::png::PngEncoder::new(fs::File::create(&path).unwrap());
		encoder.set_icc_profile(p3.encode().unwrap()).unwrap();
		encoder.write_image(&p3_pixel, 1, 1, image::ExtendedColorType::Rgba8).unwrap();
		let loaded = simple_load_image(&path, ImageFormat::Png);
		fs::remove_file(&path).unwrap();
		let (loaded, primaries) = loaded.unwrap();
		assert_eq!(primaries, WINDOW_PRIMARIES);
		let want = match WINDOW_PRIMARIES {
			Primaries::DisplayP3 => p3_pixel,
			Primaries::Srgb => skin,
		};
		let loaded = loaded.get_pixel(0, 0).0;
		for (got, want) in loaded.iter().zip(want) {
			assert!(got.abs_diff(want) <= 2, "{loaded:?} != {want:?}");
		}
	}

	/// Linear 0.5 is sRGB 188 when nothing exceeds 1.0; brighter images are compressed to fit
	/// without NaNs or overflow.
	#[test]
	fn float_images_are_tone_mapped_to_srgb() {
		let mut sdr = image::Rgba32FImage::new(1, 1);
		sdr.put_pixel(0, 0, image::Rgba([0.5, 0.5, 0.5, 1.0]));
		assert_eq!(tone_map_to_rgba8(&sdr).get_pixel(0, 0).0, [188, 188, 188, 255]);

		let mut hdr = image::Rgba32FImage::new(4, 1);
		hdr.put_pixel(0, 0, image::Rgba([0.5, 0.5, 0.5, 1.0]));
		hdr.put_pixel(1, 0, image::Rgba([8.0, 8.0, 8.0, 1.0]));
		hdr.put_pixel(2, 0, image::Rgba([f32::NAN, f32::INFINITY, -1.0, 1.0]));
		hdr.put_pixel(3, 0, image::Rgba([4.0, 0.0, 0.0, 0.5]));
		let out = tone_map_to_rgba8(&hdr);
		let mid = out.get_pixel(0, 0).0[0];
		assert!((150..188).contains(&mid), "{mid}");
		assert_eq!(out.get_pixel(1, 0).0, [255, 255, 255, 255]);
		assert_eq!(out.get_pixel(2, 0).0, [0, 255, 0, 255]);
		let red = out.get_pixel(3, 0).0;
		assert!(
			red[0] > 200 && red[1] == 0 && red[2] == 0 && (127..=128).contains(&red[3]),
			"{red:?}"
		);
	}

	/// Same as the Display P3 test above, through the 16 bit path.
	#[test]
	fn display_p3_png16_is_converted_to_window_space() {
		use image::ImageEncoder;
		use moxcms::{ColorProfile, Layout, TransformOptions};
		let skin: [u16; 4] = [224 * 257, 172 * 257, 140 * 257, u16::MAX];
		let p3 = ColorProfile::new_display_p3();
		let to_p3 = ColorProfile::new_srgb()
			.create_transform_16bit(Layout::Rgba, &p3, Layout::Rgba, TransformOptions::default())
			.unwrap();
		let mut p3_pixel = [0u16; 4];
		to_p3.transform(&skin, &mut p3_pixel).unwrap();

		let path =
			std::env::temp_dir().join(format!("emulsion-test-{}-p3-16.png", std::process::id()));
		let mut encoder = image::codecs::png::PngEncoder::new(fs::File::create(&path).unwrap());
		encoder.set_icc_profile(p3.encode().unwrap()).unwrap();
		let bytes: Vec<u8> = p3_pixel.iter().flat_map(|v| v.to_ne_bytes()).collect();
		encoder.write_image(&bytes, 1, 1, image::ExtendedColorType::Rgba16).unwrap();
		let loaded = simple_load_image(&path, ImageFormat::Png);
		fs::remove_file(&path).unwrap();
		let (loaded, primaries) = loaded.unwrap();
		assert_eq!(primaries, WINDOW_PRIMARIES);
		let want = match WINDOW_PRIMARIES {
			Primaries::DisplayP3 => p3_pixel.map(|v| ((v as u32 + 128) / 257) as u8),
			Primaries::Srgb => [224u8, 172, 140, 255],
		};
		let loaded = loaded.get_pixel(0, 0).0;
		for (got, want) in loaded.iter().zip(want) {
			assert!(got.abs_diff(want) <= 2, "{loaded:?} != {want:?}");
		}
	}

	/// The matrix the shader applies to sRGB pictures in a Display P3 window: sRGB red is
	/// (0.9175, 0.2003, 0.1386) in P3 once encoded, white stays white, and it agrees with the
	/// ICC transform used for tagged images.
	#[test]
	fn srgb_to_display_p3_matrix_maps_primaries() {
		use moxcms::{ColorProfile, Layout, TransformOptions};
		let encode = |linear: f32| {
			if linear <= 0.003_130_8 {
				12.92 * linear
			} else {
				1.055 * linear.powf(1.0 / 2.4) - 0.055
			}
		};
		let apply = |rgb: [f32; 3]| {
			let m = SRGB_TO_DISPLAY_P3;
			[0, 1, 2].map(|row| encode((0..3).map(|col| m[col][row] * rgb[col]).sum::<f32>()))
		};
		let red = apply([1.0, 0.0, 0.0]);
		for (got, want) in red.iter().zip([0.9175, 0.2003, 0.1386]) {
			assert!((got - want).abs() < 1e-3, "{red:?}");
		}
		for channel in apply([1.0; 3]) {
			assert!((channel - 1.0).abs() < 1e-5, "{channel}");
		}

		let to_p3 = ColorProfile::new_srgb()
			.create_transform_8bit(
				Layout::Rgba,
				&ColorProfile::new_display_p3(),
				Layout::Rgba,
				TransformOptions::default(),
			)
			.unwrap();
		let mut icc_red = [0u8; 4];
		to_p3.transform(&[255, 0, 0, 255], &mut icc_red).unwrap();
		for (icc, matrix) in icc_red.iter().zip(red) {
			assert!(icc.abs_diff((matrix * 255.0).round() as u8) <= 1, "{icc_red:?} vs {red:?}");
		}
	}

	/// sRGB is what the window shows without help on every platform, so an sRGB tag costs no
	/// conversion and the picture is left to the shader.
	#[test]
	fn srgb_tagged_png_is_left_alone() {
		use image::ImageEncoder;
		let path =
			std::env::temp_dir().join(format!("emulsion-test-{}-srgb.png", std::process::id()));
		let mut encoder = image::codecs::png::PngEncoder::new(fs::File::create(&path).unwrap());
		encoder.set_icc_profile(moxcms::ColorProfile::new_srgb().encode().unwrap()).unwrap();
		encoder.write_image(&[224, 172, 140, 255], 1, 1, image::ExtendedColorType::Rgba8).unwrap();
		let loaded = simple_load_image(&path, ImageFormat::Png);
		fs::remove_file(&path).unwrap();
		let (loaded, primaries) = loaded.unwrap();
		assert_eq!(primaries, Primaries::Srgb);
		assert_eq!(loaded.get_pixel(0, 0).0, [224, 172, 140, 255]);
	}

	/// Primaries alone do not make an sRGB or Display P3 profile: the tone curve counts too.
	#[test]
	fn profiles_with_other_tone_curves_are_converted() {
		use moxcms::{ColorProfile, curve_from_gamma};
		let skipped =
			|profile: &ColorProfile| profile_conversion(&profile.encode().unwrap()).1.is_none();
		assert!(skipped(&ColorProfile::new_srgb()));
		assert_eq!(
			skipped(&ColorProfile::new_display_p3()),
			WINDOW_PRIMARIES == Primaries::DisplayP3
		);

		// P3 primaries with gamma 2.6.
		assert!(!skipped(&ColorProfile::new_dci_p3()));
		let mut srgb_gamma_22 = ColorProfile::new_srgb();
		srgb_gamma_22.red_trc = Some(curve_from_gamma(2.2));
		srgb_gamma_22.green_trc = srgb_gamma_22.red_trc.clone();
		srgb_gamma_22.blue_trc = srgb_gamma_22.red_trc.clone();
		assert!(!skipped(&srgb_gamma_22));
		let mut p3_gamma_26 = ColorProfile::new_display_p3();
		p3_gamma_26.red_trc = Some(curve_from_gamma(2.6));
		p3_gamma_26.green_trc = p3_gamma_26.red_trc.clone();
		p3_gamma_26.blue_trc = p3_gamma_26.red_trc.clone();
		assert!(!skipped(&p3_gamma_26));
	}

	#[test]
	fn svg_render_size_fits_the_box_within_the_caps() {
		assert_eq!(svg_render_size(100.0, 50.0, (1000, 1000)), (1000, 500));
		assert_eq!(svg_render_size(100.0, 50.0, (1000, 100)), (200, 100));
		let (width, height) = svg_render_size(100.0, 50.0, (100_000, 100_000));
		assert!(u64::from(width) * u64::from(height) <= 64 * 1024 * 1024);
		assert!(width.abs_diff(2 * height) <= 1);
		assert_eq!(svg_render_size(10.0, 1000.0, (100_000, 100_000)).1, 16384);
	}

	/// RAW files are TIFF containers that only the macOS decoder can read; anywhere else they
	/// must not be listed as images. HEIC is listed on macOS only.
	#[test]
	fn raw_and_heic_are_supported_on_macos_only() {
		for name in ["a.CR2", "a.cr3", "a.nef", "a.arw", "a.DNG", "a.raf", "a.orf", "a.rw2"]
			.into_iter()
			.chain(["a.pef", "a.srw", "a.heic", "a.HEIF", "a.hif"])
		{
			assert_eq!(is_file_supported(Path::new(name)), cfg!(target_os = "macos"), "{name}");
		}
	}

	/// The extension check never touches the file: these paths don't exist.
	#[test]
	fn image_extensions_are_recognised_without_reading_the_file() {
		assert!(has_image_extension(Path::new("/nope/photo.JPG")));
		assert!(has_image_extension(Path::new("/nope/anim.gif")));
		assert!(!has_image_extension(Path::new("/nope/notes.txt")));
		assert!(!has_image_extension(Path::new("/nope/no_extension")));
	}

	#[cfg(target_os = "macos")]
	#[test]
	fn heic_is_detected_by_magic_and_by_extension() {
		let dir = std::env::temp_dir();
		let pid = std::process::id();
		let by_magic = dir.join(format!("emulsion-test-{pid}-heic-magic.bin"));
		let by_extension = dir.join(format!("emulsion-test-{pid}-heic-ext.HEIC"));
		let mut header = vec![0u8; 600];
		header[..12].copy_from_slice(b"\0\0\0\x18ftypheic");
		fs::write(&by_magic, &header).unwrap();
		fs::write(&by_extension, [0u8; 600]).unwrap();
		let magic_format = detect_format(&by_magic);
		let extension_format = detect_format(&by_extension);
		fs::remove_file(&by_magic).unwrap();
		fs::remove_file(&by_extension).unwrap();
		assert!(matches!(magic_format.unwrap(), ImgFormat::System));
		assert!(matches!(extension_format.unwrap(), ImgFormat::System));
	}

	/// Workers must take the priority request first, and anything once it is cleared.
	#[test]
	fn loader_queue_honours_priority() {
		let shared = LoaderShared {
			running: AtomicBool::new(true),
			queue: Mutex::new(
				(1..=3).map(|req_id| LoadRequest::new(req_id, PathBuf::new(), false)).collect(),
			),
			wake: Condvar::new(),
			priority: AtomicU32::new(NON_EXISTENT_REQUEST_ID),
		};
		shared.set_priority(2);
		assert_eq!(shared.next_request().unwrap().req_id, 2);
		shared.clear_priority(2);
		assert_eq!(shared.next_request().unwrap().req_id, 1);
		shared.running.store(false, Ordering::Release);
		assert!(shared.next_request().is_none());
	}

	/// A prefetch decodes the first frame only, and says whether it left frames out.
	#[test]
	fn first_frame_only_reports_left_out_frames() {
		let frames =
			|count: u32| (0..count).map(|_| Ok(LoadResult::Done { req_id: 0, truncated: false }));
		let run = |count, all_frames| {
			let mut seen = 0;
			let truncated = process_frames(frames(count), all_frames, |_| {
				seen += 1;
				Ok(())
			})
			.unwrap();
			(seen, truncated)
		};
		assert_eq!(run(3, false), (1, true));
		assert_eq!(run(1, false), (1, false));
		assert_eq!(run(3, true), (3, false));
	}
}
