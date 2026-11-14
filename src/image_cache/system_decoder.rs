//! Decoding through Apple's ImageIO, for formats the `image` crate can't read: HEIC/HEIF (and
//! AVIF when our own decoder isn't compiled in) and camera RAW.
//!
//! Pixels are drawn into a Display P3 bitmap context, the gamut of the window: wide-gamut
//! sources (Display P3 HEIC) keep their colours and sRGB ones are converted to P3 (the loader
//! flags the result `Primaries::DisplayP3`). The EXIF orientation is reported by ImageIO itself
//! (it also accounts for the HEIF `irot`/`imir` transforms) and is *not* applied to the pixels.

use std::ffi::c_void;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use objc2_core_foundation::{
	CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType, CFURL, CGPoint, CGRect, CGSize,
};
use objc2_core_graphics::{
	CGBitmapContextCreate, CGColorSpace, CGContext, CGImage, CGImageAlphaInfo,
	kCGColorSpaceDisplayP3,
};
use objc2_image_io::{
	CGImageSource, kCGImagePropertyOrientation, kCGImageSourceCreateThumbnailFromImageAlways,
	kCGImageSourceCreateThumbnailFromImageIfAbsent, kCGImageSourceCreateThumbnailWithTransform,
	kCGImageSourceThumbnailMaxPixelSize,
};

use gelatin::image;

use super::{ImageLoaderError, Orientation, Result};

/// Largest edge of a RAW file's preview shown in the main view.
const RAW_MAIN_VIEW_SIZE: u32 = 4096;

fn error(description: &'static str) -> ImageLoaderError {
	ImageLoaderError { description: description.into() }
}

/// Decodes a whole HEIC/HEIF/AVIF image, or the embedded preview of a RAW file (`raw`).
///
/// RAW files are never developed in full: that takes seconds per file, while the camera's
/// embedded JPEG preview is instant and at most `RAW_MAIN_VIEW_SIZE` pixels wide.
pub fn load(path: &Path, raw: bool) -> Result<(image::RgbaImage, Orientation)> {
	if raw { decode(path, Some(RAW_MAIN_VIEW_SIZE), true) } else { decode(path, None, false) }
}

/// Decodes an image that fits into `size` x `size`, without applying its orientation.
/// RAW files use their embedded preview when they have one.
pub fn load_thumbnail(
	path: &Path,
	raw: bool,
	size: u32,
) -> Result<(image::RgbaImage, Orientation)> {
	decode(path, Some(size), raw)
}

fn decode(
	path: &Path,
	max_size: Option<u32>,
	prefer_embedded: bool,
) -> Result<(image::RgbaImage, Orientation)> {
	let bytes = path.as_os_str().as_bytes();
	// SAFETY: `bytes` is valid for `bytes.len()` bytes for the duration of the call.
	let url = unsafe {
		CFURL::from_file_system_representation(None, bytes.as_ptr(), bytes.len() as isize, false)
	}
	.ok_or_else(|| error("invalid file path"))?;
	// SAFETY: no options are passed.
	let source = unsafe { CGImageSource::with_url(&url, None) }
		.ok_or_else(|| error("ImageIO doesn't recognise the file"))?;
	// SAFETY: the options dictionary is built from CF types only.
	let cg_image = unsafe {
		match max_size {
			None => source.image_at_index(0, None),
			Some(size) => source
				.thumbnail_at_index(0, Some(thumbnail_options(size, prefer_embedded).as_opaque())),
		}
	}
	.ok_or_else(|| error("ImageIO failed to decode the image"))?;
	Ok((render_display_p3(&cg_image)?, orientation(&source)))
}

fn thumbnail_options(
	max_size: u32,
	prefer_embedded: bool,
) -> CFRetained<CFDictionary<CFString, CFType>> {
	let yes = CFBoolean::new(true);
	let no = CFBoolean::new(false);
	let max_size = CFNumber::new_i32(i32::try_from(max_size).unwrap_or(i32::MAX));
	// SAFETY: reading ImageIO's constant keys.
	unsafe {
		let create_key = if prefer_embedded {
			kCGImageSourceCreateThumbnailFromImageIfAbsent
		} else {
			kCGImageSourceCreateThumbnailFromImageAlways
		};
		CFDictionary::from_slices(
			&[
				create_key,
				kCGImageSourceThumbnailMaxPixelSize,
				kCGImageSourceCreateThumbnailWithTransform,
			],
			&[yes.as_ref(), max_size.as_ref(), no.as_ref()],
		)
	}
}

/// The EXIF orientation as ImageIO reports it, [`Orientation::Deg0`] when there is none.
fn orientation(source: &CGImageSource) -> Orientation {
	// SAFETY: no options are passed; the orientation property is a CFNumber.
	let exif_orientation = unsafe {
		source.properties_at_index(0, None).and_then(|properties| {
			properties
				.cast_unchecked::<CFString, CFNumber>()
				.get(kCGImagePropertyOrientation)
				.and_then(|number| number.as_i32())
		})
	};
	match exif_orientation {
		Some(2) => Orientation::Deg0HorFlip,
		Some(3) => Orientation::Deg180,
		Some(4) => Orientation::Deg180HorFlip,
		Some(5) => Orientation::Deg90VerFlip,
		Some(6) => Orientation::Deg270,
		Some(7) => Orientation::Deg270VerFlip,
		Some(8) => Orientation::Deg90,
		_ => Orientation::Deg0,
	}
}

/// Draws the image into an 8-bit Display P3 bitmap and returns it with straight
/// (non-premultiplied) alpha, which is what the textures and shaders expect. Display P3 is the
/// gamut of the window on macOS, and it uses the sRGB transfer curve like the textures do.
fn render_display_p3(cg_image: &CGImage) -> Result<image::RgbaImage> {
	let width = CGImage::width(Some(cg_image));
	let height = CGImage::height(Some(cg_image));
	let too_big = || error("image is too large");
	let bytes_per_row = width.checked_mul(4).ok_or_else(too_big)?;
	let mut pixels = vec![0u8; bytes_per_row.checked_mul(height).ok_or_else(too_big)?];
	// SAFETY: reading a constant name.
	let display_p3 = CGColorSpace::with_name(Some(unsafe { kCGColorSpaceDisplayP3 }))
		.ok_or_else(|| error("Display P3 color space unavailable"))?;
	// SAFETY: `pixels` is `height` rows of `bytes_per_row` bytes and outlives `context`.
	let context = unsafe {
		CGBitmapContextCreate(
			pixels.as_mut_ptr().cast::<c_void>(),
			width,
			height,
			8,
			bytes_per_row,
			Some(&display_p3),
			CGImageAlphaInfo::PremultipliedLast.0,
		)
	}
	.ok_or_else(|| error("failed to create a bitmap context"))?;
	let rect = CGRect::new(CGPoint::ZERO, CGSize::new(width as f64, height as f64));
	CGContext::draw_image(Some(&context), rect, Some(cg_image));
	drop(context);
	for pixel in pixels.as_chunks_mut::<4>().0 {
		match pixel[3] {
			255 => {}
			0 => pixel[..3].fill(0),
			alpha => {
				let alpha = u32::from(alpha);
				for channel in &mut pixel[..3] {
					*channel = ((u32::from(*channel) * 255 + alpha / 2) / alpha).min(255) as u8;
				}
			}
		}
	}
	image::RgbaImage::from_raw(width as u32, height as u32, pixels).ok_or_else(too_big)
}

#[cfg(test)]
mod tests {
	use std::fs;
	use std::path::PathBuf;
	use std::process::Command;

	use image::ImageEncoder;

	use super::*;

	/// Scratch file that is removed when dropped.
	struct Scratch(PathBuf);
	impl Scratch {
		fn new(name: &str) -> Self {
			Scratch(std::env::temp_dir().join(format!("emulsion-sd-{}-{name}", std::process::id())))
		}
	}
	impl Drop for Scratch {
		fn drop(&mut self) {
			let _ = fs::remove_file(&self.0);
		}
	}

	const P3_PROFILE: &str = "/System/Library/ColorSync/Profiles/Display P3.icc";

	/// Left half `left`, right half `right`.
	fn halves(left: [u8; 4], right: [u8; 4]) -> image::RgbaImage {
		image::RgbaImage::from_fn(64, 32, |x, _| image::Rgba(if x < 32 { left } else { right }))
	}

	/// Converts to HEIC with `sips`; `None` (skip the test) when it's missing or refuses.
	fn to_heic(input: &Path, output: &Path, extra_args: &[&str]) -> Option<()> {
		let status = Command::new("sips")
			.args(["-s", "format", "heic"])
			.args(extra_args)
			.arg(input)
			.arg("--out")
			.arg(output)
			.output()
			.ok()?
			.status;
		status.success().then_some(())
	}

	fn assert_close(got: [u8; 4], want: [u8; 4]) {
		for (g, w) in got.iter().zip(want) {
			assert!(g.abs_diff(w) <= 8, "{got:?} != {want:?}");
		}
	}

	/// An sRGB colour as the Display P3 values the decoder is expected to return for it.
	fn srgb_to_p3(srgb: [u8; 4]) -> [u8; 4] {
		use moxcms::{ColorProfile, Layout, TransformOptions};
		let to_p3 = ColorProfile::new_srgb()
			.create_transform_8bit(
				Layout::Rgba,
				&ColorProfile::new_display_p3(),
				Layout::Rgba,
				TransformOptions::default(),
			)
			.unwrap();
		let mut p3 = [0u8; 4];
		to_p3.transform(&srgb, &mut p3).unwrap();
		p3
	}

	#[test]
	fn heic_decodes_with_dimensions_and_colours() {
		let (png, heic) = (Scratch::new("a.png"), Scratch::new("a.heic"));
		halves([200, 30, 30, 255], [30, 30, 200, 255]).save(&png.0).unwrap();
		if to_heic(&png.0, &heic.0, &[]).is_none() {
			return;
		}
		let (image, orientation) = load(&heic.0, false).unwrap();
		assert_eq!(image.dimensions(), (64, 32));
		assert!(matches!(orientation, Orientation::Deg0));
		assert_close(image.get_pixel(8, 16).0, srgb_to_p3([200, 30, 30, 255]));
		assert_close(image.get_pixel(56, 16).0, srgb_to_p3([30, 30, 200, 255]));

		let (thumbnail, _) = load_thumbnail(&heic.0, false, 16).unwrap();
		assert_eq!(thumbnail.dimensions(), (16, 8));
		assert_close(thumbnail.get_pixel(2, 4).0, srgb_to_p3([200, 30, 30, 255]));
	}

	/// A Display P3 HEIC comes back as the Display P3 values it holds, not converted to sRGB.
	#[test]
	fn display_p3_heic_keeps_its_display_p3_values() {
		use moxcms::{ColorProfile, Layout, TransformOptions};
		let skin = [240u8, 90, 20, 255];
		let p3 = ColorProfile::new_display_p3();
		let to_p3 = ColorProfile::new_srgb()
			.create_transform_8bit(Layout::Rgba, &p3, Layout::Rgba, TransformOptions::default())
			.unwrap();
		let mut p3_pixel = [0u8; 4];
		to_p3.transform(&skin, &mut p3_pixel).unwrap();
		// Far enough apart that skipping the conversion can't pass the tolerance below.
		assert!(p3_pixel.iter().zip(skin).any(|(p, s)| p.abs_diff(s) > 20), "{p3_pixel:?}");

		let Ok(profile) = fs::read(P3_PROFILE) else {
			return;
		};
		let (png, heic) = (Scratch::new("p3.png"), Scratch::new("p3.heic"));
		let pixels: Vec<u8> = p3_pixel.iter().copied().cycle().take(64 * 32 * 4).collect();
		let mut encoder = image::codecs::png::PngEncoder::new(fs::File::create(&png.0).unwrap());
		encoder.set_icc_profile(profile).unwrap();
		encoder.write_image(&pixels, 64, 32, image::ExtendedColorType::Rgba8).unwrap();
		if to_heic(&png.0, &heic.0, &[]).is_none() {
			return;
		}
		assert_close(load(&heic.0, false).unwrap().0.get_pixel(32, 16).0, p3_pixel);
	}

	/// ImageIO reports the EXIF orientation of the source instead of rotating the pixels.
	#[test]
	fn heic_reports_exif_orientation_without_rotating() {
		let (jpeg, heic) = (Scratch::new("o6.jpg"), Scratch::new("o6.heic"));
		let mut jpeg_bytes = Vec::new();
		image::DynamicImage::from(halves([200, 30, 30, 255], [30, 30, 200, 255]))
			.to_rgb8()
			.write_to(&mut std::io::Cursor::new(&mut jpeg_bytes), image::ImageFormat::Jpeg)
			.unwrap();
		// Big-endian TIFF with a single Orientation = 6 entry, in an APP1 segment after SOI.
		let exif = b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0";
		let mut with_exif = jpeg_bytes[..2].to_vec();
		with_exif.extend_from_slice(&[0xff, 0xe1]);
		with_exif.extend_from_slice(&(exif.len() as u16 + 2).to_be_bytes());
		with_exif.extend_from_slice(exif);
		with_exif.extend_from_slice(&jpeg_bytes[2..]);
		fs::write(&jpeg.0, with_exif).unwrap();
		if to_heic(&jpeg.0, &heic.0, &[]).is_none() {
			return;
		}
		let (image, orientation) = load(&heic.0, false).unwrap();
		assert_eq!(image.dimensions(), (64, 32));
		assert!(matches!(orientation, Orientation::Deg270), "{orientation:?}");
	}

	/// A HEIF `irot` box is already applied to the pixels ImageIO returns, so it must not be
	/// reported as an orientation as well.
	#[test]
	fn heic_rotation_box_is_not_applied_twice() {
		let (png, heic) = (Scratch::new("r90.png"), Scratch::new("r90.heic"));
		halves([200, 30, 30, 255], [30, 30, 200, 255]).save(&png.0).unwrap();
		if to_heic(&png.0, &heic.0, &["-r", "90"]).is_none() {
			return;
		}
		let (image, orientation) = load(&heic.0, false).unwrap();
		assert_eq!(image.dimensions(), (32, 64));
		assert!(matches!(orientation, Orientation::Deg0), "{orientation:?}");
		let (thumbnail, orientation) = load_thumbnail(&heic.0, false, 16).unwrap();
		assert_eq!(thumbnail.dimensions(), (8, 16));
		assert!(matches!(orientation, Orientation::Deg0), "{orientation:?}");
	}
}
