//! Optional filmstrip of the current folder, drawn as an overlay above the bottom controls.
//!
//! Thumbnails are decoded on a worker thread, uploaded lazily for the cells that are actually
//! on screen, and evicted with a byte-budgeted LRU so a huge folder cannot grow without bound.

use std::{
	cell::{Cell, RefCell},
	collections::HashMap,
	path::{Path, PathBuf},
	rc::Rc,
	sync::{
		Arc, Mutex,
		mpsc::{Receiver, Sender, channel},
	},
	thread,
};

use gelatin::{
	DrawContext, Event, EventKind, NextUpdate, Widget, WidgetData, WidgetError,
	add_common_widget_functions,
	cgmath::Matrix4,
	glium::{
		Blend, BlendingFunction, Frame, LinearBlendingFactor, Surface,
		texture::SrgbTexture2d,
		uniform,
		uniforms::{MagnifySamplerFilter, MinifySamplerFilter, SamplerWrapFunction},
	},
	image,
	misc::{Alignment, Length, LogicalRect, LogicalVector, WidgetPlacement},
	window::RenderValidity,
	winit::event::{ElementState, MouseButton},
};

#[cfg(target_os = "macos")]
use crate::image_cache::image_loader::system_decoder;
use crate::image_cache::image_loader::{
	ImgFormat, Orientation, Primaries, WINDOW_PRIMARIES, change_primaries, convert_to_window_space,
	detect_format, detect_orientation, load_svg,
};

/// Cell edge in logical pixels, excluding the gap between cells.
pub const DEFAULT_THUMBNAIL_SIZE: f32 = 96.0;
const CELL_GAP: f32 = 4.0;
const SELECTION_BORDER: f32 = 2.0;
/// The one non-grey colour of the UI, converted to the window's gamut once.
static SELECTION_COLOR: std::sync::LazyLock<[f32; 4]> =
	std::sync::LazyLock::new(|| Primaries::window_color([0.95, 0.72, 0.25, 1.0]));

/// How the strip behaves: hidden entirely, revealed with the bottom controls, or always shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StripeMode {
	Off,
	AutoHide,
	Pinned,
}

impl StripeMode {
	pub fn parse(value: &str) -> Self {
		match value {
			"auto" | "autohide" => Self::AutoHide,
			"pinned" | "on" => Self::Pinned,
			_ => Self::Off,
		}
	}

	pub fn as_str(self) -> &'static str {
		match self {
			Self::Off => "off",
			Self::AutoHide => "auto",
			Self::Pinned => "pinned",
		}
	}

	pub fn next(self) -> Self {
		match self {
			Self::Off => Self::AutoHide,
			Self::AutoHide => Self::Pinned,
			Self::Pinned => Self::Off,
		}
	}
}

/// Upper bound of the decoded-thumbnail cache: 5% of physical memory, but never less than
/// 64 MiB and never more than 512 MiB.
fn thumbnail_cache_budget() -> usize {
	const MIB: usize = 1024 * 1024;
	let total = crate::system_memory::total_memory_bytes().unwrap_or(0) as usize;
	(total / 20).clamp(64 * MIB, 512 * MIB)
}

struct Request {
	index: usize,
	path: PathBuf,
	size: u32,
	generation: u32,
}

struct Decoded {
	index: usize,
	image: image::RgbaImage,
	generation: u32,
}

struct Entry {
	texture: SrgbTexture2d,
	bytes: usize,
	/// Value of the draw counter when this thumbnail was last shown, for LRU eviction.
	last_used: Cell<u64>,
}

struct StripeData {
	placement: WidgetPlacement,
	drawn_bounds: LogicalRect,
	visible: bool,
	render_validity: RenderValidity,

	cell_size: f32,
	current_index: usize,
	entry_count: usize,
	/// Paths handed over by the playback manager for the indices we asked about.
	paths: HashMap<usize, PathBuf>,

	textures: HashMap<usize, Entry>,
	/// Bumped whenever the folder contents change; decodes tagged with an older value are stale.
	generation: u32,
	/// Folder the cached thumbnails belong to.
	folder: Option<PathBuf>,
	/// Counts draws, to stamp the thumbnails that were on screen.
	draw_tick: Cell<u64>,
	cached_bytes: usize,
	budget: usize,

	requested: HashMap<usize, ()>,
	request_tx: Sender<Request>,
	decoded_rx: Receiver<Decoded>,

	on_select: Option<Rc<dyn Fn(usize)>>,
}

impl WidgetData for StripeData {
	fn placement(&mut self) -> &mut WidgetPlacement {
		&mut self.placement
	}
	fn drawn_bounds(&mut self) -> &mut LogicalRect {
		&mut self.drawn_bounds
	}
	fn visible(&mut self) -> &mut bool {
		&mut self.visible
	}
}

pub struct ThumbnailStripe {
	data: RefCell<StripeData>,
	/// Indices the widget wants a path for; the owner fills them in through `set_paths`.
	wanted: RefCell<Vec<usize>>,
	needs_paths: Cell<bool>,
}

impl ThumbnailStripe {
	pub fn new(cell_size: f32) -> Self {
		let (request_tx, request_rx) = channel::<Request>();
		let (decoded_tx, decoded_rx) = channel::<Decoded>();
		// Filling a strip means decoding a dozen full-size images, so share the queue between
		// a few workers; more than four only competes with the full-image loader.
		let request_rx = Arc::new(Mutex::new(request_rx));
		let workers =
			thread::available_parallelism().map_or(2, |cores| cores.get() / 2).clamp(1, 4);
		for worker in 0..workers {
			let requests = request_rx.clone();
			let decoded = decoded_tx.clone();
			thread::Builder::new()
				.name(format!("thumbnail-decoder-{worker}"))
				.spawn(move || decode_loop(&requests, &decoded))
				.expect("could not start the thumbnail decoder thread");
		}

		let placement = WidgetPlacement {
			width: Length::Stretch { min: 0.0, max: f32::INFINITY },
			height: Length::Fixed(cell_size + 2.0 * CELL_GAP),
			horizontal_align: Alignment::Center,
			vertical_align: Alignment::End,
			ignore_layout: true,
			..Default::default()
		};

		Self {
			data: RefCell::new(StripeData {
				placement,
				drawn_bounds: Default::default(),
				visible: false,
				render_validity: Default::default(),
				cell_size,
				current_index: 0,
				entry_count: 0,
				paths: HashMap::new(),
				textures: HashMap::new(),
				generation: 0,
				folder: None,
				draw_tick: Cell::new(0),
				cached_bytes: 0,
				budget: thumbnail_cache_budget(),
				requested: HashMap::new(),
				request_tx,
				decoded_rx,
				on_select: None,
			}),
			wanted: RefCell::new(Vec::new()),
			needs_paths: Cell::new(false),
		}
	}

	add_common_widget_functions!(data);

	pub fn set_on_select<T: Fn(usize) + 'static>(&self, callback: T) {
		self.data.borrow_mut().on_select = Some(Rc::new(callback));
	}

	pub fn set_bottom_offset(&self, offset: f32) {
		let mut data = self.data.borrow_mut();
		if data.placement.margin_bottom != offset {
			data.placement.margin_bottom = offset;
			data.render_validity.invalidate();
		}
	}

	/// Tell the strip which folder position is shown; forgets everything when the folder
	/// (or its length) changes, since the indices then mean different files.
	///
	/// `folder` is the directory of the shown entry, if known yet.
	pub fn set_folder_state(
		&self,
		current_index: usize,
		entry_count: usize,
		folder: Option<&Path>,
	) {
		let mut data = self.data.borrow_mut();
		let folder_changed = folder.is_some() && data.folder.as_deref() != folder;
		if folder_changed || data.entry_count != entry_count {
			if folder_changed {
				data.folder = folder.map(Path::to_path_buf);
			}
			data.entry_count = entry_count;
			data.generation = data.generation.wrapping_add(1);
			data.paths.clear();
			data.requested.clear();
			data.textures.clear();
			data.cached_bytes = 0;
			self.wanted.borrow_mut().clear();
			self.needs_paths.set(false);
			data.render_validity.invalidate();
		}
		if data.current_index != current_index {
			data.current_index = current_index;
			data.render_validity.invalidate();
		}
	}

	/// Indices whose path the widget still needs, if any.
	pub fn take_path_requests(&self) -> Option<Vec<usize>> {
		if !self.needs_paths.replace(false) {
			return None;
		}
		let wanted = std::mem::take(&mut *self.wanted.borrow_mut());
		if wanted.is_empty() { None } else { Some(wanted) }
	}

	pub fn set_paths(&self, paths: Vec<(usize, PathBuf)>) {
		let mut data = self.data.borrow_mut();
		let size = data.cell_size.ceil() as u32 * 2;
		let generation = data.generation;
		for (index, path) in paths {
			if data.requested.insert(index, ()).is_none() {
				let _ =
					data.request_tx.send(Request { index, path: path.clone(), size, generation });
			}
			data.paths.insert(index, path);
		}
	}
	pub fn contains_point(&self, point: LogicalVector) -> bool {
		let data = self.data.borrow();
		data.visible && data.drawn_bounds.contains(point)
	}

	/// Uploads decoded thumbnails and reports whether anything changed.
	pub fn poll(&self, context_display: &gelatin::Display) -> bool {
		let mut data = self.data.borrow_mut();
		let mut changed = false;
		while let Ok(decoded) = data.decoded_rx.try_recv() {
			if decoded.generation != data.generation {
				// Decoded for a folder we have since left; its index means another file now.
				continue;
			}
			let (w, h) = decoded.image.dimensions();
			let raw = gelatin::glium::texture::RawImage2d::from_raw_rgba(
				decoded.image.into_raw(),
				(w, h),
			);
			if let Ok(texture) = SrgbTexture2d::new(context_display, raw) {
				let bytes = (w * h * 4) as usize;
				let last_used = Cell::new(data.draw_tick.get());
				if let Some(replaced) =
					data.textures.insert(decoded.index, Entry { texture, bytes, last_used })
				{
					data.cached_bytes -= replaced.bytes;
				}
				data.cached_bytes += bytes;
				changed = true;
			}
		}
		while data.cached_bytes > data.budget {
			let used = data.textures.iter().map(|(&index, entry)| (index, entry.last_used.get()));
			let Some(victim) = eviction_victim(used, data.current_index) else {
				break;
			};
			if let Some(entry) = data.textures.remove(&victim) {
				data.cached_bytes -= entry.bytes;
				// Forget the request and path too, so drawing asks for it again if the cell
				// comes back on screen.
				data.requested.remove(&victim);
				data.paths.remove(&victim);
			}
		}
		if changed {
			data.render_validity.invalidate();
		}
		changed
	}

	fn cells_per_row(data: &StripeData) -> usize {
		let stride = data.cell_size + CELL_GAP;
		((data.drawn_bounds.size.vec.x / stride).floor() as usize).max(1)
	}

	fn visible_range(data: &StripeData) -> (usize, usize) {
		let visible = Self::cells_per_row(data).min(data.entry_count);
		let half = visible / 2;
		let start = data.current_index.saturating_sub(half);
		let start = start.min(data.entry_count.saturating_sub(visible));
		(start, start + visible)
	}
}

/// The least recently used `(index, last_used)` entry, never the one at `keep`.
fn eviction_victim(entries: impl Iterator<Item = (usize, u64)>, keep: usize) -> Option<usize> {
	entries
		.filter(|&(index, _)| index != keep)
		.min_by_key(|&(_, last_used)| last_used)
		.map(|(index, _)| index)
}

fn decode_loop(requests: &Mutex<Receiver<Request>>, decoded: &Sender<Decoded>) {
	loop {
		// The lock is only held while taking the next request, so the decoders run in parallel.
		let request = match requests.lock() {
			Ok(guard) => guard.recv(),
			Err(_) => return,
		};
		let Ok(request) = request else {
			return;
		};
		let Some(thumbnail) = decode_thumbnail(&request) else {
			continue;
		};
		let (index, generation) = (request.index, request.generation);
		if decoded.send(Decoded { index, generation, image: thumbnail }).is_err() {
			return;
		}
	}
}

fn decode_thumbnail(request: &Request) -> Option<image::RgbaImage> {
	let format = detect_format(&request.path).ok()?;
	if matches!(format, ImgFormat::Svg) {
		let mut thumbnail = load_svg(&request.path, (request.size, request.size)).ok()?;
		change_primaries(&mut thumbnail, Primaries::Srgb, WINDOW_PRIMARIES);
		return Some(thumbnail);
	}
	#[cfg(target_os = "macos")]
	if matches!(format, ImgFormat::System | ImgFormat::Raw) {
		let raw = matches!(format, ImgFormat::Raw);
		let (thumbnail, orientation) =
			system_decoder::load_thumbnail(&request.path, raw, request.size).ok()?;
		return Some(apply_orientation(thumbnail, orientation));
	}
	use image::ImageDecoder;
	let reader = image::ImageReader::open(&request.path).ok()?.with_guessed_format().ok()?;
	let mut decoder = reader.into_decoder().ok()?;
	let icc = decoder.icc_profile().ok().flatten();
	let image = image::DynamicImage::from_decoder(decoder).ok()?;
	// `thumbnail` box-samples down first, which is ~2.5x faster than a full Triangle resize
	// and indistinguishable at cell size.
	let mut thumbnail = image.thumbnail(request.size, request.size).into_rgba8();
	// The strip is drawn without the picture shader, so the pixels have to be in the window's
	// gamut already.
	let primaries = match icc {
		Some(icc) => convert_to_window_space(&mut thumbnail, &icc),
		None => Primaries::Srgb,
	};
	change_primaries(&mut thumbnail, primaries, WINDOW_PRIMARIES);
	Some(apply_orientation(thumbnail, detect_orientation(&request.path).unwrap_or_default()))
}

/// Rotating the already downscaled thumbnail is cheap, so EXIF is honoured here rather than
/// in the sampler like the main view does.
fn apply_orientation(mut image: image::RgbaImage, orientation: Orientation) -> image::RgbaImage {
	use image::imageops::{
		flip_horizontal_in_place, flip_vertical_in_place, rotate90, rotate180_in_place, rotate270,
	};
	// `imageops` rotates clockwise, `Orientation` describes counter-clockwise rotation.
	match orientation {
		Orientation::Deg0 => image,
		Orientation::Deg0HorFlip => {
			flip_horizontal_in_place(&mut image);
			image
		}
		Orientation::Deg90 => rotate270(&image),
		Orientation::Deg90VerFlip => {
			let mut result = rotate270(&image);
			flip_vertical_in_place(&mut result);
			result
		}
		Orientation::Deg180 => {
			rotate180_in_place(&mut image);
			image
		}
		Orientation::Deg180HorFlip => {
			flip_vertical_in_place(&mut image);
			image
		}
		Orientation::Deg270 => rotate90(&image),
		Orientation::Deg270VerFlip => {
			let mut result = rotate90(&image);
			flip_vertical_in_place(&mut result);
			result
		}
	}
}

impl Widget for ThumbnailStripe {
	fn draw(&self, target: &mut Frame, context: &DrawContext) -> Result<NextUpdate, WidgetError> {
		let data = self.data.borrow();
		if !data.visible || data.entry_count == 0 {
			return Ok(NextUpdate::Latest);
		}
		let tick = data.draw_tick.get() + 1;
		data.draw_tick.set(tick);
		let draw_params = gelatin::glium::DrawParameters {
			viewport: Some(*context.viewport),
			blend: Blend {
				color: BlendingFunction::Addition {
					source: LinearBlendingFactor::SourceAlpha,
					destination: LinearBlendingFactor::OneMinusSourceAlpha,
				},
				..Default::default()
			},
			..Default::default()
		};

		let bounds = data.drawn_bounds.align_to_pixels(context.dpi_scale_factor);
		let backdrop = Matrix4::from_nonuniform_scale(bounds.size.vec.x, bounds.size.vec.y, 1.0);
		let backdrop = Matrix4::from_translation(bounds.pos.vec.extend(0.0)) * backdrop;
		let backdrop = context.projection_transform * backdrop;
		target
			.draw(
				context.unit_quad_vertices,
				context.unit_quad_indices,
				context.colored_program,
				&uniform! {
					matrix: Into::<[[f32; 4]; 4]>::into(backdrop),
					color: [0.0f32, 0.0, 0.0, 0.72],
				},
				&draw_params,
			)
			.unwrap_or_else(|error| log::error!("Could not draw the thumbnail backdrop: {error}"));

		let (start, end) = Self::visible_range(&data);
		let missing: Vec<usize> = (start..end)
			.filter(|index| !data.textures.contains_key(index) && !data.paths.contains_key(index))
			.collect();
		if !missing.is_empty() {
			*self.wanted.borrow_mut() = missing;
			self.needs_paths.set(true);
		}

		for index in start..end {
			let slot = (index - start) as f32;
			let x = bounds.pos.vec.x + CELL_GAP + slot * (data.cell_size + CELL_GAP);
			let y = bounds.pos.vec.y + CELL_GAP;

			let (cell_x, cell_y, w, h) = match data.textures.get(&index) {
				Some(entry) => {
					entry.last_used.set(tick);
					let (tex_w, tex_h) =
						(entry.texture.width() as f32, entry.texture.height() as f32);
					let scale = (data.cell_size / tex_w).min(data.cell_size / tex_h);
					let (w, h) = (tex_w * scale, tex_h * scale);
					(x + (data.cell_size - w) * 0.5, y + (data.cell_size - h) * 0.5, w, h)
				}
				None => (x, y, data.cell_size, data.cell_size),
			};

			if index == data.current_index {
				let border = Matrix4::from_nonuniform_scale(
					w + 2.0 * SELECTION_BORDER,
					h + 2.0 * SELECTION_BORDER,
					1.0,
				);
				let border = Matrix4::from_translation(
					LogicalVector::new(cell_x - SELECTION_BORDER, cell_y - SELECTION_BORDER)
						.vec
						.extend(0.0),
				) * border;
				let border = context.projection_transform * border;
				target
					.draw(
						context.unit_quad_vertices,
						context.unit_quad_indices,
						context.colored_program,
						&uniform! {
							matrix: Into::<[[f32; 4]; 4]>::into(border),
							color: *SELECTION_COLOR,
						},
						&draw_params,
					)
					.unwrap_or_else(|error| {
						log::error!("Could not draw the thumbnail selection: {error}")
					});
			}

			let cell_bg = Matrix4::from_nonuniform_scale(w, h, 1.0);
			let cell_bg =
				Matrix4::from_translation(LogicalVector::new(cell_x, cell_y).vec.extend(0.0))
					* cell_bg;
			let cell_bg = context.projection_transform * cell_bg;
			target
				.draw(
					context.unit_quad_vertices,
					context.unit_quad_indices,
					context.colored_program,
					&uniform! {
						matrix: Into::<[[f32; 4]; 4]>::into(cell_bg),
						color: [0.12f32, 0.12, 0.12, 1.0],
					},
					&draw_params,
				)
				.unwrap_or_else(|error| log::error!("Could not draw a thumbnail cell: {error}"));
			let Some(entry) = data.textures.get(&index) else {
				continue;
			};
			let transform = Matrix4::from_nonuniform_scale(w, h, 1.0);
			let transform =
				Matrix4::from_translation(LogicalVector::new(cell_x, cell_y).vec.extend(0.0))
					* transform;
			let transform = context.projection_transform * transform;
			let sampler = entry
				.texture
				.sampled()
				.wrap_function(SamplerWrapFunction::Clamp)
				.minify_filter(MinifySamplerFilter::LinearMipmapLinear)
				.magnify_filter(MagnifySamplerFilter::Linear);
			target
				.draw(
					context.unit_quad_vertices,
					context.unit_quad_indices,
					context.textured_program,
					&uniform! {
						matrix: Into::<[[f32; 4]; 4]>::into(transform),
						tex: sampler,
						color: [1.0f32, 1.0, 1.0, 1.0],
						texture_size: [w, h],
						brighten: 0.0f32,
						shadow_color: [0.0f32, 0.0, 0.0],
						shadow_offset: 1.0f32,
					},
					&draw_params,
				)
				.unwrap_or_else(|error| log::error!("Could not draw a thumbnail: {error}"));
		}
		Ok(NextUpdate::Latest)
	}

	fn layout(&self, available_space: LogicalRect) {
		let mut data = self.data.borrow_mut();
		data.default_layout(available_space);
	}

	fn handle_event(&self, event: &Event) {
		if !self.data.borrow().visible {
			return;
		}
		let EventKind::MouseButton {
			state: ElementState::Pressed, button: MouseButton::Left, ..
		} = event.kind
		else {
			return;
		};
		let (index, callback) = {
			let data = self.data.borrow();
			if !data.drawn_bounds.contains(event.cursor_pos) || data.entry_count == 0 {
				return;
			}
			let (start, end) = Self::visible_range(&data);
			let offset = event.cursor_pos.vec.x - data.drawn_bounds.pos.vec.x - CELL_GAP;
			let slot = (offset / (data.cell_size + CELL_GAP)).floor().max(0.0) as usize;
			let index = start + slot;
			if index >= end {
				return;
			}
			(index, data.on_select.clone())
		};
		if let Some(callback) = callback {
			callback(index);
		}
	}

	fn children(&self, _children: &mut Vec<Rc<dyn Widget>>) {}

	fn placement(&self) -> WidgetPlacement {
		self.data.borrow().placement
	}

	fn visible(&self) -> bool {
		self.data.borrow().visible
	}

	fn set_valid_ref(&self, render_validity: RenderValidity) {
		self.data.borrow_mut().render_validity = render_validity;
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn cache_budget_stays_between_64_and_512_mib() {
		let budget = thumbnail_cache_budget();
		assert!(budget >= 64 * 1024 * 1024);
		assert!(budget <= 512 * 1024 * 1024);
	}

	/// A 2x1 image whose left pixel is red: rotating it must move that pixel the same way the
	/// main view's orientation matrix does.
	fn marked() -> image::RgbaImage {
		image::RgbaImage::from_fn(2, 1, |x, _| {
			if x == 0 { image::Rgba([255, 0, 0, 255]) } else { image::Rgba([0, 0, 255, 255]) }
		})
	}

	#[test]
	fn exif_orientation_rotates_the_thumbnail() {
		let red = image::Rgba([255, 0, 0, 255]);

		let kept = apply_orientation(marked(), Orientation::Deg0);
		assert_eq!(kept.get_pixel(0, 0), &red);

		let turned = apply_orientation(marked(), Orientation::Deg180);
		assert_eq!(turned.dimensions(), (2, 1));
		assert_eq!(turned.get_pixel(1, 0), &red);

		// Exif 6 (Deg270 counter-clockwise) turns a landscape thumbnail into a portrait one,
		// with the left edge ending up at the top.
		let upright = apply_orientation(marked(), Orientation::Deg270);
		assert_eq!(upright.dimensions(), (1, 2));
		assert_eq!(upright.get_pixel(0, 0), &red);
	}

	#[test]
	fn eviction_takes_the_least_recently_used_and_spares_the_selection() {
		let entries = [(3, 10), (4, 2), (5, 7)];
		assert_eq!(eviction_victim(entries.into_iter(), 0), Some(4));
		assert_eq!(eviction_victim(entries.into_iter(), 4), Some(5));
		assert_eq!(eviction_victim([(4, 2)].into_iter(), 4), None);
		assert_eq!(eviction_victim(std::iter::empty(), 0), None);
	}
}
