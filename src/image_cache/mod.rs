use std::{
	borrow::Cow,
	collections::{BTreeMap, HashMap, HashSet},
	ffi::OsString,
	fs,
	path::{Path, PathBuf},
	rc::Rc,
	sync::LazyLock,
	time::SystemTime,
};

use log::trace;

use gelatin::{
	glium::{
		self, CapabilitiesSource,
		texture::{MipmapsOption, RawImage2d, SrgbTexture2d},
	},
	image,
};

pub mod image_loader;

use self::{
	directory::{DirItem, ListingChange},
	image_loader::*,
};

mod pending_requests;
use pending_requests::PendingRequests;

pub mod directory;
use directory::Directory;

#[cfg(test)]
mod tests;

#[derive(Debug, thiserror::Error)]
pub enum TextureError {
	#[error("ImageCache is waiting for the image loader to send result")]
	WaitingOnLoader,

	#[error("ImageCache is waiting for the folder listing")]
	WaitingOnListing,

	#[error("Other texture error: {0}")]
	Other(Cow<'static, str>),
}
impl From<directory::Error> for TextureError {
	fn from(value: directory::Error) -> Self {
		Self::Other(format!("Directory error: {}", value).into())
	}
}
impl From<glium::texture::TextureCreationError> for TextureError {
	fn from(value: glium::texture::TextureCreationError) -> Self {
		Self::Other(format!("glium failed to create the texture: {value}").into())
	}
}
impl From<std::io::Error> for TextureError {
	fn from(value: std::io::Error) -> Self {
		Self::Other(format!("IO error occured during texture creation: {value}").into())
	}
}
impl TextureError {
	pub fn from_failed_request(req_id: u32) -> Self {
		Self::Other(format!("Failed to load #{req_id}").into())
	}
}

pub type TextureResult<T> = std::result::Result<T, TextureError>;

#[derive(Debug, thiserror::Error)]
pub enum PathResolutionError {
	#[error(
		"No image path has been specified. (Eg emulsion was started without it being requested to open an image or folder)"
	)]
	NotYetSpecified,

	#[error(
		"Could not find file path matching the request, because the Directory Filter hasn't finished yet"
	)]
	WaitingOnDirFilter,
}

/// Some operations eg "load the next image" doesn't only have to load the image but
/// also have to determine the path to the image. It's possible that we successfuly
/// determined the path, but cannot load the image for some reason.
pub type PathedTextureResult =
	std::result::Result<(PathBuf, TextureResult<AnimationFrameTexture>), PathResolutionError>;

/// Estimate for images whose header can't be read (eg SVGs, which are rendered up to 4096 px).
const UNKNOWN_SIZE_ESTIMATE: isize = 16 * 1024 * 1024;

pub fn get_image_size_estimate(width: u32, height: u32) -> isize {
	// In an RGBA image, each pixel is 4 bytes.
	// counting all the mipmaps would add an additionnal multiplier of around ~1.6
	// but only the gpu textures have mip maps so just multiply by 1.5
	// 4 x 1.5 gives the factor 6.
	isize::try_from(width as u64 * height as u64 * 6).unwrap_or(isize::MAX)
}

pub fn get_anim_size_estimate(frames: &[AnimationFrameTexture]) -> isize {
	frames.iter().fold(0, |total: isize, frame| {
		total.saturating_add(get_image_size_estimate(frame.w, frame.h))
	})
}

/// Size a request is expected to need, read from the image header without decoding it.
fn estimate_file_size(path: &Path) -> isize {
	image::ImageReader::open(path)
		.and_then(|reader| reader.with_guessed_format())
		.ok()
		.and_then(|reader| reader.into_dimensions().ok())
		.map_or(UNKNOWN_SIZE_ESTIMATE, |(width, height)| get_image_size_estimate(width, height))
}

/// Picks which cache entries to keep from `(req_id, distance from the current image, size)`:
/// nearest first while they fit in `capacity`. The current image (distance 0) is always kept.
fn select_cache_entries(mut entries: Vec<(u32, usize, isize)>, capacity: isize) -> Vec<u32> {
	entries.sort_by_key(|&(_, distance, _)| distance);
	let mut remaining = capacity;
	entries
		.into_iter()
		.filter_map(|(req_id, distance, size)| {
			if distance == 0 || remaining > size.saturating_add(UNKNOWN_SIZE_ESTIMATE) {
				remaining = remaining.saturating_sub(size);
				Some(req_id)
			} else {
				None
			}
		})
		.collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum SendOutcome {
	/// A request went out to the loader.
	Sent,
	/// Nothing to do for this file: it is cached, already requested or unchanged.
	Skipped,
	/// No more requests can be sent for now: too many are pending or the budget is used up.
	Blocked,
}

/// The request sender function must process all prefetched requests to avoid
/// hitting the pending request limit (MAX_PENDING_REQUESTS). The display is needed
/// for this processing but it would be incorrect to require the dispaly for prefetch
/// requests
enum RequestKind<'a> {
	NonPriority,
	Priority { display: &'a gelatin::Display },
}

impl<'a> RequestKind<'a> {
	pub fn priority(self) -> bool {
		match self {
			RequestKind::Priority { .. } => true,
			RequestKind::NonPriority => false,
		}
	}
}

pub struct TextureGridItem {
	pub tex: SrgbTexture2d,
	pub col: u32,
	pub row: u32,
}

#[derive(Clone)]
pub struct AnimationFrameTexture {
	/// The maximum texture size supported by GPUs is limited. However it may be
	/// the I want to view a 16k*16k image while my GPU only supports 4k*4k
	/// textures. To work around this, we split up large images into a grid of
	/// smaller ones, which are displayed to appear as one continous surface.
	pub tex_grid: Rc<Vec<TextureGridItem>>,
	/// Number of physical pixels between two adjacent cells in one dimension.
	/// For example the pixel offset from the corner of the image to the corner
	/// of the cell at the 3rd column and 2nd row is
	/// (3*cell_step_size, 2*cell_step_size)
	pub cell_step_size: u32,

	pub delay_nano: u64,
	pub orientation: Orientation,

	/// The total width of the image. This equals to the sum of the widths of the
	/// textures from a single row of the grid
	pub w: u32,
	/// The total height of the image. This equals to the sum of the heights of the
	/// textures from a single column of the grid
	pub h: u32,

	/// The gamut the texels are encoded in. The shader converts it to the window's.
	pub primaries: Primaries,
	/// Rendered from an SVG, at a size that can be made sharper by rendering it again.
	pub svg: bool,
}
impl AnimationFrameTexture {
	pub fn from_image(
		display: &gelatin::Display,
		image: image::RgbaImage,
		delay_nano: u64,
		orientation: Orientation,
		primaries: Primaries,
		svg: bool,
	) -> TextureResult<Self> {
		let (w, h) = image.dimensions();
		let img_bytes = image.into_raw();
		let mut tex_grid = Vec::new();

		// The reasoning behind dividing by 2 and taking the min with 4*1024, is
		// that if the textures are going to be swaped out from GPU memory it
		// might be easier to shuffle smaller chunks of memory around. (Because
		// I believe that if the memory is fragmented, it is easier to find
		// space for a smaller texture)
		let max_size = (display.get_capabilities().max_texture_size as u32 / 2).min(4 * 1024);

		let grid_cols = ((w - 1) / max_size) + 1;
		let grid_rows = ((h - 1) / max_size) + 1;

		for row in 0..grid_rows {
			for col in 0..grid_cols {
				let offset_x = col * max_size;
				let offset_y = row * max_size;
				let cell_w = (w - offset_x).min(max_size);
				let cell_h = (h - offset_y).min(max_size);
				let tex = texture_from_img_rect(
					display, w, h, &img_bytes, offset_x, offset_y, cell_w, cell_h,
				)?;
				let item = TextureGridItem { tex, col, row };
				tex_grid.push(item);
			}
		}

		Ok(AnimationFrameTexture {
			tex_grid: Rc::new(tex_grid),
			delay_nano,
			orientation,
			primaries,
			svg,
			w,
			h,
			cell_step_size: max_size,
		})
	}

	pub fn oriented_dimensions(&self) -> (u32, u32) {
		use Orientation::*;
		match self.orientation {
			Deg0 | Deg0HorFlip | Deg180 | Deg180HorFlip => (self.w, self.h),
			Deg90 | Deg90VerFlip | Deg270 | Deg270VerFlip => (self.h, self.w),
		}
	}
}

/// Whether any texel of the rgba8 buffer is not fully opaque; stops at the first one.
fn has_transparency(rgba: &[u8]) -> bool {
	rgba.as_chunks::<4>().0.iter().any(|texel| texel[3] != 255)
}

const LINEAR_STEPS: usize = 4096;

static SRGB_TO_LINEAR: LazyLock<[f32; 256]> = LazyLock::new(|| {
	std::array::from_fn(|i| {
		let c = i as f32 / 255.0;
		if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
	})
});

static LINEAR_TO_SRGB: LazyLock<[u8; LINEAR_STEPS]> = LazyLock::new(|| {
	std::array::from_fn(|i| {
		let l = i as f32 / (LINEAR_STEPS - 1) as f32;
		let c = if l <= 0.003_130_8 { l * 12.92 } else { 1.055 * l.powf(1.0 / 2.4) - 0.055 };
		(c * 255.0 + 0.5) as u8
	})
});

/// Premultiplies an rgba8 buffer holding sRGB-encoded color. The GPU filters sRGB textures in
/// linear light, so the multiplication has to happen in linear light too for the fragment
/// shader's `rgb / a` to recover the original color. Opaque texels are left untouched.
fn premultiply_srgb(rgba: &mut [u8]) {
	let decode = &*SRGB_TO_LINEAR;
	let encode = &*LINEAR_TO_SRGB;
	for texel in rgba.as_chunks_mut::<4>().0 {
		if texel[3] == 255 {
			continue;
		}
		let alpha = texel[3] as f32 / 255.0;
		for channel in &mut texel[..3] {
			let linear = decode[*channel as usize] * alpha;
			*channel = encode[(linear * (LINEAR_STEPS - 1) as f32 + 0.5) as usize];
		}
	}
}

#[cfg(test)]
mod premultiply_tests {
	use super::*;

	#[test]
	fn opaque_buffers_are_not_transparent_and_stay_untouched() {
		let mut opaque = vec![10, 20, 30, 255, 200, 100, 0, 255];
		assert!(!has_transparency(&opaque));
		premultiply_srgb(&mut opaque);
		assert_eq!(opaque, [10, 20, 30, 255, 200, 100, 0, 255]);
	}

	#[test]
	fn one_translucent_texel_is_detected() {
		assert!(has_transparency(&[1, 2, 3, 255, 1, 2, 3, 254]));
	}

	#[test]
	fn premultiplying_scales_in_linear_light() {
		let mut texels = vec![255, 255, 255, 0, 255, 255, 255, 128, 40, 50, 60, 255];
		premultiply_srgb(&mut texels);
		// Fully transparent texels lose their color.
		assert_eq!(texels[..4], [0, 0, 0, 0]);
		// White at 50% alpha is 0.5 in linear light, which is ~188 in sRGB.
		assert!((i32::from(texels[4]) - 188).abs() <= 2, "{}", texels[4]);
		assert_eq!(texels[4], texels[5]);
		assert_eq!(texels[7], 128);
		assert_eq!(texels[8..], [40, 50, 60, 255]);
	}
}

/// img_bytes has to be an rgba8 buffer.
#[allow(clippy::too_many_arguments)]
fn texture_from_img_rect(
	display: &gelatin::Display,
	img_w: u32,
	img_h: u32,
	img_bytes: &[u8],
	offset_x: u32,
	offset_y: u32,
	cell_w: u32,
	cell_h: u32,
) -> TextureResult<SrgbTexture2d> {
	debug_assert!(offset_x + cell_w <= img_w && offset_y + cell_h <= img_h);
	let mut raw_image = if img_w == cell_w {
		assert!(offset_x == 0);
		let start = (offset_y as usize * img_w as usize) * 4;
		let end = start + (cell_h as usize * cell_w as usize * 4);
		RawImage2d {
			data: Cow::Borrowed(&img_bytes[start..end]),
			format: glium::texture::ClientFormat::U8U8U8U8,
			width: cell_w,
			height: cell_h,
		}
	} else {
		let cell_size = cell_w as usize * cell_h as usize * 4;
		let mut cell_pixels = Vec::with_capacity(cell_size);
		for y in offset_y..(offset_y + cell_h) {
			// We multiply by four becase we need to convert from a pixel offset to
			// a byte offset and each pixel is 4 bytes wide.
			let start = (y as usize * img_w as usize + offset_x as usize) * 4;
			let end = start + (cell_w as usize * 4);
			cell_pixels.extend_from_slice(&img_bytes[start..end]);
		}
		RawImage2d::from_raw_rgba(cell_pixels, (cell_w, cell_h))
	};

	// Straight alpha can't be filtered: transparent texels' RGB would bleed into the edges of
	// every mip level. Opaque images (the common case) only pay for the scan.
	if has_transparency(&raw_image.data) {
		premultiply_srgb(raw_image.data.to_mut());
	}

	// A full chain, so far zoomed-out huge images are properly filtered.
	let mipmaps = if cell_w.min(cell_h) < 2 {
		MipmapsOption::NoMipmap
	} else {
		MipmapsOption::AutoGeneratedMipmaps
	};
	Ok(SrgbTexture2d::with_mipmaps(display, raw_image, mipmaps)?)
}

struct CachedTexture {
	/// Contains the load request id
	_req_id: u32,
	needs_update: bool,
	mod_time: Option<SystemTime>,

	/// This is false if there are frames from the animation that haven't been added.
	/// This is used when requesting a frame that's outside of `frames`.
	/// In such a case the value of `fully_loaded` is inspected and if the image
	/// is loaded the frame number is wrapped around so that it falls onto the range.
	/// If it's not fully loaded yet a `WaitingOnLoader` error is returned.
	fully_loaded: bool,

	/// Only the first frame of an animation was loaded (by a prefetch); the rest has to be
	/// loaded once the image is shown. Such an image is not a finished still.
	truncated: bool,

	/// - `false` if loading is still in progress or if succeeded.
	/// - `true` if this failed to load,
	failed: bool,

	/// An SVG is being rendered again (sharper): `frames` still holds the old render, which the
	/// new one replaces when it arrives. Until then the image is not handed out again, so the
	/// callers keep showing the old one.
	replacing: bool,

	/// If the target file is an image this vector will have a single texture once the
	/// image uploaded to the GPU. If the target file is an animated image like a gif,
	/// these the frames
	frames: Vec<AnimationFrameTexture>,
}

/// The process of loading an image (or animation frame) consists of the following steps.
/// Note that even still images are handled as 1 frame long animations as there is
/// semantically no difference between those and this keeps the code relatively simple.
///
/// - ImageCache - Load request sent
///     - An entry representing the request is created in `ImageCache::ongoing_requests`
/// - Worker thread - Decodes an image from the byte stream into a CPU-side buffer. Send this back on a channel
///     - Decoded pixel data on channel (CPU)
/// - ImageCache - Prefetched image received from worker thread
///     - Decoded pixel data in `ImageCache::prefetched`
/// - ImageCache - Prefetched image is uploaded to a GPU texture from the CPU
///     - Pixel data is on the gpu referred to by `ImageCache::texture_cache`
///
pub struct ImageCache {
	dir: Directory,

	current_frame_idx: usize,

	total_capacity: isize,

	pending_requests: PendingRequests,
	texture_cache: BTreeMap<u32, CachedTexture>,
	loader: ImageLoader,
	/// The box SVG requests are rendered to fit, see `set_svg_view`.
	svg_view: (u32, u32),
	/// What `estimate_file_size` found for a request id: it reads the file, and prefetching
	/// asks again at every tick while the budget is used up.
	size_estimates: HashMap<u32, isize>,
}

/// This is a store for the supported images loaded from a folder
///
/// The basic idea is to have a few images already in the memory while an image is shown on the screen
impl ImageCache {
	const MAX_PENDING_REQUESTS: usize = 5;

	/// # Arguments
	/// * `capacity` - Number of bytes. The last image loaded will be the one at which the allocated memory reaches or exceeds capacity
	pub fn new(capacity: isize, threads: u32) -> ImageCache {
		ImageCache {
			dir: Directory::new(),
			current_frame_idx: 0,

			total_capacity: capacity,

			pending_requests: PendingRequests::new(),
			texture_cache: BTreeMap::new(),
			loader: ImageLoader::new(threads),
			svg_view: SVG_DEFAULT_FIT,
			size_estimates: HashMap::new(),
		}
	}

	/// Sets the box, in physical pixels, that SVGs loaded from now on are rendered to fit: the
	/// part of the window the picture is shown in, at most `SVG_INITIAL_MAX_SIDE` on a side.
	/// Zooming in is answered with sharper renders (see `rerender_svg`).
	pub fn set_svg_view(&mut self, width: u32, height: u32) {
		let clamp = |side: u32| side.clamp(1, SVG_INITIAL_MAX_SIDE);
		self.svg_view = (clamp(width), clamp(height));
	}

	/// Renders the SVG that is shown again, to fit `fit` (physical pixels). The texture shown
	/// so far stays in use until the new one is uploaded: `try_getting_requested_image` answers
	/// `WaitingOnLoader` meanwhile, so the caller has to ask for the current image again.
	/// Returns `false` when that can't be done now (the image is not a loaded SVG, the loader
	/// is busy or has results of this file left): try again later.
	pub fn rerender_svg(&mut self, fit: (u32, u32)) -> bool {
		let Some(DirItem { path, request_id }) = self.curr_dir_item() else {
			return false;
		};
		if self.pending_requests.len() >= Self::MAX_PENDING_REQUESTS
			|| self.pending_requests.get_all_ids().contains(&request_id)
		{
			return false;
		}
		let Some(texture) = self.texture_cache.get_mut(&request_id) else {
			return false;
		};
		if !texture.fully_loaded || texture.failed {
			return false;
		}
		let Some(frame) = texture.frames.first().filter(|frame| frame.svg) else {
			return false;
		};
		let (width, height) = svg_render_size(frame.w as f32, frame.h as f32, fit);
		let reserved = get_image_size_estimate(width, height);
		texture.replacing = true;
		let mut request = LoadRequest::new(request_id, path, false);
		request.svg_fit = fit;
		self.pending_requests.add_request(&request, reserved);
		self.loader.set_priority(request_id);
		self.loader.send_load_request(request);
		true
	}

	/// Capacity not used by cached textures or reserved by requests that are still loading.
	fn remaining_capacity(&self) -> isize {
		let cached = self.texture_cache.values().fold(0isize, |total, texture| {
			total.saturating_add(get_anim_size_estimate(&texture.frames))
		});
		self.total_capacity
			.saturating_sub(cached)
			.saturating_sub(self.pending_requests.reserved_total())
			.max(0)
	}

	pub fn current_filename(&self) -> Option<OsString> {
		self.dir.curr_filename()
	}

	pub fn current_file_path(&self) -> Option<PathBuf> {
		self.current_filename().map(|name| self.dir.path().join(name))
	}

	/// Returns `None` while the very first folder is being listed.
	pub fn current_file_index(&mut self) -> Option<usize> {
		self.sync_directory();
		self.dir.curr_img_index()
	}

	/// Returns `None` while the very first folder is being listed.
	pub fn current_dir_len(&mut self) -> Option<usize> {
		self.sync_directory();
		self.dir.image_count()
	}

	fn curr_dir_item(&self) -> Option<DirItem> {
		self.dir.curr_descriptor().cloned()
	}

	/// Returns tru if and only if the current image has been fully loaded and it has a single frame.
	pub fn loaded_still_image(&self) -> bool {
		if let Some(desc) = self.dir.curr_descriptor()
			&& let Some(img) = self.texture_cache.get(&desc.request_id)
			&& img.fully_loaded
			&& img.frames.len() == 1
			&& !img.truncated
		{
			return true;
		}
		false
	}

	/// True while loaded results are waiting to be received or uploaded.
	pub fn has_pending_requests(&self) -> bool {
		self.pending_requests.has_work()
	}

	/// Starts listing the shown folder again in the background. When the listing arrives (see
	/// `listing_pending`) it replaces the current one; the current file stays current, or the
	/// image that took its place if the file is gone.
	pub fn update_directory(&mut self) {
		self.dir.update_directory();
	}

	/// Whether a folder listing is still being produced in the background.
	pub fn listing_pending(&mut self) -> bool {
		self.sync_directory();
		self.dir.listing_pending()
	}

	/// Takes in the folder listing the background thread has finished, if any. What belongs to
	/// files that are no longer listed is dropped, their pending decodes are cancelled.
	fn sync_directory(&mut self) {
		let Some(change) = self.dir.poll_listing() else {
			return;
		};
		let dir = &self.dir;
		self.texture_cache.retain(|req_id, _| dir.has_request(*req_id));
		self.pending_requests.retain(|req_id| dir.has_request(req_id));
		self.size_estimates.retain(|req_id, _| dir.has_request(*req_id));

		// The files may have changed since they were decoded: they are all
		// checked against the modification time of the file system file.
		for texture in self.texture_cache.values_mut() {
			texture.needs_update = true;
		}
		if change == ListingChange::NewDirectory {
			self.current_frame_idx = 0;
		}
	}

	/// Path of the folder entry at `index`, or `None` if there is no such image.
	pub fn path_by_index(&mut self, index: usize) -> Option<PathBuf> {
		self.sync_directory();
		self.dir.image_by_index(index).map(|item| item.path.clone())
	}

	pub fn load_at_index(
		&mut self,
		display: &gelatin::Display,
		index: usize,
		frame_id: Option<isize>,
	) -> PathedTextureResult {
		self.sync_directory();
		let path = self
			.dir
			.image_by_index(index)
			.ok_or(PathResolutionError::WaitingOnDirFilter)?
			.path
			.clone();

		let result = self.load_synced(display, &path, frame_id);
		Ok((path, result))
	}

	/// Returns `Err(TextureError::WaitingOnLoader)` while the image is loading and
	/// `Err(TextureError::WaitingOnListing)` while the folder of `path` is being listed.
	pub fn load_specific(
		&mut self,
		display: &gelatin::Display,
		path: &Path,
		frame_id: Option<isize>,
	) -> TextureResult<AnimationFrameTexture> {
		self.sync_directory();
		self.load_synced(display, path, frame_id)
	}

	/// `load_specific` for callers that have just called `sync_directory`: the listing must
	/// not change between resolving `path` and loading it.
	fn load_synced(
		&mut self,
		display: &gelatin::Display,
		path: &Path,
		frame_id: Option<isize>,
	) -> TextureResult<AnimationFrameTexture> {
		trace!("Begin `load_specific`");
		self.receive_prefetched();
		trace!("Receive prefetched done");
		let (target_file_name, parent) = if path.is_dir() {
			(None, path.to_owned())
		} else {
			let (file_name, parent) = get_file_name_and_parent(path)?;
			(Some(file_name), parent)
		};

		let prev_img_index = self.dir.curr_img_index();
		let listed = match &target_file_name {
			Some(file_name) => self.dir.change_directory_with_filename(&parent, file_name)?,
			None => self.dir.change_directory(&parent)?,
		};
		if !listed {
			// The opened file is decoded meanwhile; the previous listing stays in use and the
			// caller asks again.
			if let Some(file_name) = &target_file_name {
				let file_path = parent.join(file_name);
				if has_image_extension(&file_path) {
					let req_id = self.dir.reserve_request_id(file_path.clone());
					self.send_request_for_file(
						file_path,
						req_id,
						RequestKind::Priority { display },
					);
				}
			}
			return Err(TextureError::WaitingOnListing);
		}
		if target_file_name.is_none() {
			self.current_frame_idx = 0;
		}
		if let Some(img_index) = self.dir.curr_img_index() {
			self.dir.set_curr_img_index(img_index)?;
		}
		let requested_frame_id = match frame_id {
			Some(frame_id) => frame_id,
			None => {
				let mut retval = 0;
				if let Some(prev_img_index) = prev_img_index
					&& let Some(curr_img_index) = self.dir.curr_img_index()
					&& curr_img_index == prev_img_index
				{
					retval = self.current_frame_idx as isize;
				}
				retval
			}
		};
		// Evicting textures stalls the frame by a few ms. When the target is cached already
		// nothing is added, so that waits for the next `prefetch_neighbors`.
		let target_cached = self
			.curr_dir_item()
			.is_some_and(|item| self.texture_cache.contains_key(&item.request_id));
		if !target_cached {
			self.refresh_cache();
		}
		self.try_getting_requested_image(display, requested_frame_id)
	}

	fn refresh_cache(&mut self) {
		trace!("Begin `refresh_cache`");
		let (Some(curr_index), Some(count)) = (self.dir.curr_img_index(), self.dir.image_count())
		else {
			return;
		};
		// Entries of files that are no longer images of the folder are dropped. The others are
		// ordered by how many images they are from the current one (the folder wraps around).
		let entries = self
			.texture_cache
			.iter()
			.filter_map(|(&req_id, texture)| {
				let index = self.dir.img_index_of_request(req_id)?;
				let distance = index.abs_diff(curr_index);
				let size = get_anim_size_estimate(&texture.frames);
				Some((req_id, distance.min(count - distance), size))
			})
			.collect();
		let capacity = self.total_capacity.saturating_sub(self.pending_requests.reserved_total());
		let keep: HashSet<u32> = select_cache_entries(entries, capacity).into_iter().collect();
		self.texture_cache.retain(|req_id, _| keep.contains(req_id));
	}

	pub fn load_next(&mut self, display: &gelatin::Display) -> PathedTextureResult {
		self.load_jump(display, 1, 0)
	}
	pub fn load_prev(&mut self, display: &gelatin::Display) -> PathedTextureResult {
		self.load_jump(display, -1, 0)
	}

	pub fn load_jump(
		&mut self,
		display: &gelatin::Display,
		file_jump_count: i32,
		frame_jump_count: isize,
	) -> PathedTextureResult {
		self.sync_directory();
		if file_jump_count == 0 {
			// Here, it is possible that the current image was already
			// requested but not yet loaded.
			let target_frame = self.current_frame_idx as isize + frame_jump_count;
			let requested = self.try_getting_requested_image(display, target_frame);
			if let Some(path) = self.current_file_path() {
				return Ok((path, requested));
			} else {
				return Err(PathResolutionError::NotYetSpecified);
			}
		} else {
			self.current_frame_idx = 0;
		}

		let target_path;
		if file_jump_count.abs() == 1 {
			if file_jump_count > 0 {
				self.dir.jump_to_next();
			} else {
				self.dir.jump_to_prev();
			}
			target_path = self
				.dir
				.curr_descriptor()
				.ok_or(PathResolutionError::NotYetSpecified)?
				.path
				.clone();
		} else if let (Some(curr_index), Some(img_count)) =
			(self.dir.curr_img_index(), self.dir.image_count())
			&& img_count > 0
		{
			// rem_euclid calculates the least nonnegative remainder
			let target_index = (curr_index as isize + file_jump_count as isize)
				.rem_euclid(img_count as isize) as usize;

			target_path = self
				.dir
				.image_by_index(target_index)
				.ok_or(PathResolutionError::NotYetSpecified)?
				.path
				.clone();
		} else {
			log::info!(
				"Folder is empty, no folder was open, or folder hasn't finished filtering when trying to jump to an image by index."
			);
			return Err(PathResolutionError::NotYetSpecified);
		}
		let result = self.load_synced(display, &target_path, None);
		Ok((target_path, result))
	}

	fn receive_prefetched(&mut self) {
		use std::sync::mpsc::TryRecvError;
		loop {
			match self.loader.try_recv_prefetched() {
				Ok(load_result) => {
					self.pending_requests.add_load_result(load_result);
				}
				Err(TryRecvError::Disconnected) => {
					log::error!("The image loader threads stopped unexpectedly.");
					break;
				}
				Err(TryRecvError::Empty) => break,
			}
		}
	}

	pub fn process_prefetched(&mut self, display: &gelatin::Display) -> TextureResult<()> {
		self.receive_prefetched();
		let mut uploaded_one = false;
		let req_ids = self.pending_requests.get_all_ids();
		let mut retval = Ok(());
		for id in req_ids {
			if let Some(results) = self.pending_requests.take_results(id) {
				for result in results {
					match self.upload_to_texture(display, result) {
						Ok(_) => uploaded_one = true,
						// it's okay to ignore if the image falied to load here, this is just pre-fetch.
						// so we don't break the loop
						Err(e) => {
							retval = Err(e);
						}
					}
				}
			}

			// Clippy suggests an alternative that does not compile so we allow this case
			#[allow(clippy::question_mark)]
			if retval.is_err() {
				return retval;
			}
			if uploaded_one {
				break;
			}
		}
		Ok(())
	}

	/// This funciton will check if the image which we are at, is already avaialbe.
	///
	///
	///
	/// Negative frame numbers are allowed. So are larger-than-total-frame-count frame numbers.
	///
	/// This is because this function call will often happen when the animation is not fully loaded yet.
	/// So in order to minimize complexity of the functions calling this one, frame ids that are out of
	/// bounds are allowed and will be wraped around if needed within this function.
	fn try_getting_requested_image(
		&mut self,
		display: &gelatin::Display,
		frame_id: isize,
	) -> TextureResult<AnimationFrameTexture> {
		trace!("Begin `try_getting_requested_image` in `image_cache`");
		let DirItem { path, request_id: req_id } = self
			.curr_dir_item()
			.ok_or_else(|| TextureError::Other("Could not get path for current image".into()))?;

		// Check if it's among the prefetched, and upload it, if it is
		if let Some(results) = self.pending_requests.take_results(req_id) {
			for load_result in results {
				self.upload_to_texture(display, load_result)?;
			}
			// And just let the next blok deal with locating the appropriate frame.
		}

		// Prefetching decoded only the first frame of an animation: load the rest now.
		if self.texture_cache.get(&req_id).is_some_and(|tex| tex.truncated) {
			self.send_request_for_file(path.clone(), req_id, RequestKind::Priority { display });
		}

		// Check if it is inside the texture cache first
		if let Some(tex) = self.texture_cache.get(&req_id) {
			if tex.failed {
				return Err(TextureError::from_failed_request(req_id));
			}
			let modified = fs::metadata(&path).ok().and_then(|m| m.modified().ok());
			// A file that changed since it was decoded has to be decoded again.
			let stale = matches!((modified, tex.mod_time), (Some(now), Some(then)) if now != then);
			if !stale {
				if tex.replacing {
					return Err(TextureError::WaitingOnLoader);
				}
				let count = tex.frames.len() as isize;
				if tex.fully_loaded && count == 0 {
					return Err(TextureError::from_failed_request(req_id));
				}
				if count > 0 && (tex.fully_loaded || (frame_id >= 0 && frame_id < count)) {
					let wrapped_id =
						if frame_id < 0 { count + (frame_id % count) } else { frame_id % count };
					if let Some(frame) = tex.frames.get(wrapped_id as usize) {
						self.current_frame_idx = wrapped_id as usize;
						return Ok(frame.clone());
					}
				}
				return Err(TextureError::WaitingOnLoader);
			}
			self.texture_cache.remove(&req_id);
		}
		if self.pending_requests.contains(&req_id) {
			self.loader.set_priority(req_id);
			return Err(TextureError::WaitingOnLoader);
		}
		self.send_request_for_file(path, req_id, RequestKind::Priority { display });
		// If the texture is not in the cache just throw our hands in the air
		// and tell the caller that we gotta wait for the loader to load this texture.
		Err(TextureError::WaitingOnLoader)
	}

	fn upload_to_texture(
		&mut self,
		display: &gelatin::Display,
		load_result: LoadResult,
	) -> TextureResult<Option<AnimationFrameTexture>> {
		use std::collections::btree_map::Entry;
		match load_result {
			LoadResult::Start { req_id, metadata } => {
				let curr_mod_time = metadata.modified().ok();
				if !self.pending_requests.contains(&req_id) {
					return Ok(None);
				}
				match self.texture_cache.entry(req_id) {
					Entry::Vacant(entry) => {
						entry.insert(CachedTexture {
							_req_id: req_id,
							needs_update: false,
							fully_loaded: false,
							mod_time: curr_mod_time,
							failed: false,
							truncated: false,
							replacing: false,
							frames: Vec::new(),
						});
					}
					Entry::Occupied(mut entry) => {
						// Loaded again: the file changed, or only its first frame was loaded.
						let cached = entry.get_mut();
						// A sharper render of an SVG replaces its frame when it arrives.
						if !cached.replacing {
							cached.frames.clear();
							cached.fully_loaded = false;
							cached.truncated = false;
							cached.mod_time = curr_mod_time;
						}
					}
				}
				Ok(None)
			}
			LoadResult::Frame { req_id, image, delay_nano, orientation, primaries, svg } => {
				if !self.pending_requests.contains(&req_id) {
					return Ok(None);
				}
				if let Some(entry) = self.texture_cache.get_mut(&req_id) {
					let anim_frame = AnimationFrameTexture::from_image(
						display,
						image,
						delay_nano,
						orientation,
						primaries,
						svg,
					)?;
					if entry.replacing {
						entry.replacing = false;
						entry.frames.clear();
					}
					entry.frames.push(anim_frame.clone());
					return Ok(Some(anim_frame));
				}
				Ok(None)
			}
			LoadResult::Done { req_id, truncated } => {
				if let Some(tex) = self.texture_cache.get_mut(&req_id) {
					tex.fully_loaded = true;
					tex.truncated = truncated;
					tex.replacing = false;
				}
				self.loader.clear_priority(req_id);
				self.pending_requests.set_finished(&req_id);
				Ok(None)
			}
			LoadResult::Failed { req_id } => {
				self.loader.clear_priority(req_id);
				self.pending_requests.set_finished(&req_id);
				if let Some(tex) = self.texture_cache.get_mut(&req_id) {
					if tex.replacing {
						// The sharper render failed: the one on screen stays.
						tex.replacing = false;
						return Ok(None);
					}
					tex.fully_loaded = true;
					tex.failed = true;
				}
				Err(TextureError::from_failed_request(req_id))
			}
		}
	}

	/// Requests the images around the current one, nearest first, alternating sides
	/// (+1, -1, +2, -2, ...) and wrapping around like next/previous do. Images that are
	/// already cached or requested are skipped; it stops when the budget or the limit
	/// of pending requests is reached. Nothing is requested before the current image is
	/// loaded: prefetches would take the decoder threads and the pending-request slots the
	/// shown image needs, and delay it.
	pub fn prefetch_neighbors(&mut self) {
		self.sync_directory();
		let (Some(curr_index), Some(count)) = (self.dir.curr_img_index(), self.dir.image_count())
		else {
			return;
		};
		let current_loaded = self.curr_dir_item().is_some_and(|item| {
			self.texture_cache
				.get(&item.request_id)
				.is_some_and(|tex| tex.fully_loaded && !tex.truncated && !tex.replacing)
		});
		if !current_loaded {
			return;
		}
		self.refresh_cache();
		for index in prefetch_order(curr_index, count) {
			if self.prefetch_index(index) == SendOutcome::Blocked {
				return;
			}
		}
	}

	/// Returns true if a request was sent.
	pub fn prefetch_at_index(&mut self, index: usize) -> bool {
		self.sync_directory();
		self.prefetch_index(index) == SendOutcome::Sent
	}

	fn prefetch_index(&mut self, index: usize) -> SendOutcome {
		let params =
			self.dir.image_by_index(index).map(|desc| (desc.path.clone(), desc.request_id));
		match params {
			Some((path, req_id)) => {
				self.send_request_for_file(path, req_id, RequestKind::NonPriority)
			}
			None => SendOutcome::Blocked,
		}
	}

	/// Priority requests ignore the cache capacity, prefetch requests are only sent while their
	/// estimated size fits in it. The estimate is reserved until the request finishes.
	fn send_request_for_file(
		&mut self,
		file_path: PathBuf,
		req_id: u32,
		kind: RequestKind,
	) -> SendOutcome {
		if let RequestKind::Priority { display } = kind
			&& self.pending_requests.len() >= Self::MAX_PENDING_REQUESTS
			&& let Err(e) = self.process_prefetched(display)
		{
			log::error!("Error while processing prefetched images:\n{}", e);
		}
		if self.pending_requests.len() >= Self::MAX_PENDING_REQUESTS {
			return SendOutcome::Blocked;
		}
		let priority = kind.priority();
		let mut cache_enty_invalid = false;
		if let Some(texture) = self.texture_cache.get_mut(&req_id) {
			if priority && texture.truncated {
				// Prefetching left out the frames after the first one, they are wanted now.
			} else if !texture.needs_update {
				return SendOutcome::Skipped;
			} else {
				texture.needs_update = false;
				if let Some(existing_mod_time) = texture.mod_time {
					let new_mod_time =
						fs::metadata(&file_path).ok().and_then(|m| m.modified().ok());
					if let Some(new_mod_time) = new_mod_time {
						if new_mod_time == existing_mod_time {
							return SendOutcome::Skipped;
						} else {
							cache_enty_invalid = true;
						}
					}
				}
			}
		}
		if cache_enty_invalid {
			self.texture_cache.remove(&req_id);
			self.size_estimates.remove(&req_id);
		}
		if priority {
			self.loader.set_priority(req_id);
		}
		if self.pending_requests.contains(&req_id) {
			return SendOutcome::Skipped;
		}
		let reserved =
			*self.size_estimates.entry(req_id).or_insert_with(|| estimate_file_size(&file_path));
		if !priority && reserved > self.remaining_capacity() {
			return SendOutcome::Blocked;
		}
		if priority
			&& let Some(texture) = self.texture_cache.get_mut(&req_id)
			&& texture.truncated
		{
			// The animation is loaded again from the start. Until its first frame arrives
			// the cached one keeps being served.
			texture.truncated = false;
			texture.fully_loaded = false;
		}
		// Prefetches only decode the first frame of animations.
		let mut request = LoadRequest::new(req_id, file_path, priority);
		request.svg_fit = self.svg_view;
		self.pending_requests.add_request(&request, reserved);
		self.loader.send_load_request(request);
		SendOutcome::Sent
	}
}

/// The other indices of `count` images in the order they are prefetched around `curr`: nearest
/// first, alternating forward and back (+1, -1, +2, -2, ...), wrapping around, each once.
fn prefetch_order(curr: usize, count: usize) -> impl Iterator<Item = usize> {
	(1..=count / 2)
		.flat_map(move |offset| [(curr + offset) % count, (curr + count - offset) % count])
		// With an even count the last offset reaches the same image from both sides.
		.take(count.saturating_sub(1))
}

fn get_file_name_and_parent(path: &Path) -> std::io::Result<(OsString, PathBuf)> {
	use std::io;

	let file_name = match path.file_name() {
		Some(f) => f.to_owned(),
		None => {
			return Err(io::Error::other(format!("Could not get file name from path {:?}", path)));
		}
	};
	let parent = match path.parent() {
		Some(p) => {
			if p == Path::new("") {
				Path::new(".").canonicalize()?
			} else {
				p.canonicalize()?
			}
		}
		None => {
			let mut path = path.canonicalize()?;
			if !path.pop() {
				return Err(io::Error::other(format!(
					"Could not get parent directory of {:?}",
					path
				)));
			}
			path
		}
	};

	Ok((file_name, parent))
}

#[cfg(test)]
mod eviction_tests {
	use super::*;

	#[test]
	fn nearest_entries_survive_and_current_is_always_kept() {
		const SIZE: isize = 10;
		// (req_id, distance, size): req ids deliberately not in distance order.
		let entries = vec![(1, 5, SIZE), (2, 1, SIZE), (3, 0, SIZE), (4, 2, SIZE), (5, 1, SIZE)];
		// Room for the current image and two entries next to it, not for the one two steps away.
		let capacity = 3 * SIZE + SIZE / 2 + UNKNOWN_SIZE_ESTIMATE;
		assert_eq!(select_cache_entries(entries, capacity), vec![3, 2, 5]);

		// An image bigger than the whole capacity is still kept when it is the current one.
		assert_eq!(select_cache_entries(vec![(7, 0, isize::MAX)], 1), vec![7]);
	}
}
