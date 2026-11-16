use std::{
	cell::{Ref, RefCell},
	path::PathBuf,
	rc::{Rc, Weak},
	sync::{Arc, Mutex, mpsc},
	time::{Duration, Instant},
};

use gelatin::{
	cgmath::{Matrix4, Vector2, Vector3},
	glium::{Frame, Program, Surface, uniform, uniforms::MagnifySamplerFilter},
	shaders::ShaderDescriptor,
	winit::{
		event::{ElementState, MouseButton},
		platform::modifier_supplement::KeyEventExtModifierSupplement,
	},
};

use gelatin::{
	Display, DrawContext, Event, EventKind, NextUpdate, Widget, WidgetData, WidgetError,
	add_common_widget_functions,
	application::request_exit,
	misc::{Alignment, Length, LogicalRect, LogicalVector, WidgetPlacement},
	window::{RenderValidity, Window},
	winit::keyboard::ModifiersState,
};

use crate::{
	clipboard_handler::ClipboardHandler,
	configuration::{
		Antialias, Cache, Configuration, DEFAULT_GAMMA, clamp_gamma, gamma_from_slider_position,
	},
	image_cache::{
		AnimationFrameTexture,
		image_loader::{Orientation, svg_render_size},
	},
	input_handling::*,
	playback_manager::*,
	shaders,
	utils::{physical_key_to_string, virtual_keycode_to_string},
};

use super::{
	bottom_bar::{BOTTOM_BAR_HEIGHT, BottomBar},
	copy_notification::CopyNotifications,
	help_screen::HelpScreen,
	thumbnail_stripe::{StripeMode, ThumbnailStripe},
};

const MIN_ZOOM_FACTOR: f32 = 0.0001;
const MAX_ZOOM_FACTOR: f32 = 10000.0;
const AA_TEXEL_SIZE_THRESHOLD: f32 = 4f32;
const MACOS_DRAG_STRIP_HEIGHT: f32 = 28.0;

/// An instant far enough in the past that no click is "recent" relative to it.
fn long_ago() -> Instant {
	let now = Instant::now();
	now.checked_sub(Duration::from_secs(10)).unwrap_or(now)
}

/// Continuous mip level for an image drawn with `texel_size` physical pixels per texel: 0 while
/// magnified or 1:1, then one level per halving. It is fed to `textureLod` with trilinear
/// filtering, so levels blend smoothly instead of popping.
fn lod_for_texel_size(texel_size: f32) -> f32 {
	if texel_size > 0.0 { (-texel_size.log2()).max(0.0) } else { 0.0 }
}

/// How texels are filtered when the picture is magnified: `Auto` shows real pixels (nearest) once
/// a texel covers `AA_TEXEL_SIZE_THRESHOLD` physical pixels and smooth (linear) below that.
fn magnify_filter(antialiasing: Antialias, texel_size: f32) -> MagnifySamplerFilter {
	match antialiasing {
		Antialias::Auto if texel_size < AA_TEXEL_SIZE_THRESHOLD => MagnifySamplerFilter::Linear,
		Antialias::Auto | Antialias::Never => MagnifySamplerFilter::Nearest,
		Antialias::Always => MagnifySamplerFilter::Linear,
	}
}

/// The texel size after a zoom step of `delta` (wheel or key input; positive zooms in). It
/// snaps to exactly 1:1 when close to it and stays within the zoom limits.
fn zoomed_texel_size(texel_size: f32, delta: f32) -> f32 {
	let delta = if delta > 0.0 { delta + 1.0 } else { 1.0 / (delta.abs() + 1.0) };
	let new_size = (texel_size * delta).max(0.0);
	if (new_size - 1.0).abs() < 0.01 {
		1.0
	} else {
		new_size.clamp(MIN_ZOOM_FACTOR, MAX_ZOOM_FACTOR)
	}
}

/// The image centre after the picture is scaled by `ratio` about `anchor`. Both are relative
/// to the picture widget, so the point of the image under `anchor` stays under it.
fn zoomed_img_pos(img_pos: LogicalVector, anchor: LogicalVector, ratio: f32) -> LogicalVector {
	ratio * (img_pos - anchor) + anchor
}

/// An SVG texture is magnified up to this many physical pixels per texel before it is rendered
/// again.
const SVG_SHARPEN_TEXEL_SIZE: f32 = 1.5;
/// How long the zoom has to rest before a sharper render of an SVG is asked for.
const SVG_SETTLE: Duration = Duration::from_millis(150);

/// The box (physical pixels) to render an SVG again to when its `texture` (width, height) is
/// shown with `texel_size` physical pixels per texel and is blurry that way. `None` when it
/// is sharp enough, or when the size caps leave no notably bigger render to get.
fn sharper_svg_size(texture: (u32, u32), texel_size: f32) -> Option<(u32, u32)> {
	if texel_size <= SVG_SHARPEN_TEXEL_SIZE {
		return None;
	}
	let (width, height) = (texture.0 as f32, texture.1 as f32);
	let fit = ((width * texel_size).ceil() as u32, (height * texel_size).ceil() as u32);
	let (target_width, target_height) = svg_render_size(width, height, fit);
	(target_width.max(target_height) as f32 > width.max(height) * 1.05).then_some(fit)
}

#[cfg(target_os = "macos")]
fn macos_side_button_direction(button: MouseButton) -> Option<i32> {
	match button {
		MouseButton::Back | MouseButton::Other(3) => Some(-1),
		MouseButton::Forward | MouseButton::Other(4) => Some(1),
		_ => None,
	}
}
/// Horizontal scroll needed for one image step. Swipes and free-spinning thumb wheels emit
/// many small deltas, so they are accumulated instead of navigating per event.
const SWIPE_STEP: f32 = 2.0;

/// Consumes one step worth of horizontal scroll, returning -1 (previous) or 1 (next).
fn take_swipe_step(accumulated: &mut f32) -> Option<i32> {
	if accumulated.abs() < SWIPE_STEP {
		return None;
	}
	let direction = if *accumulated > 0.0 { -1 } else { 1 };
	*accumulated = 0.0;
	Some(direction)
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum ScalingMode {
	Fixed,
	FitStretch,
	FitMin,
}

#[derive(PartialEq, Eq, Clone, Copy)]
pub enum MovementDir {
	None,
	Positive,
	Negative,
}

impl MovementDir {
	fn moving(self) -> bool {
		!matches!(self, MovementDir::None)
	}
}

#[derive(Debug, Clone)]
enum HoverState {
	None,
	ItemHovered { prev_path: LoadedImgPath },
}

fn orientation_to_matrix(orientation: Orientation) -> Matrix4<f32> {
	#[rustfmt::skip]
	let result = match orientation {
		Orientation::Deg0 => Matrix4::from_scale(1.0),
		Orientation::Deg0HorFlip => Matrix4::new(
			-1.0, 0.0, 0.0, 0.0,
			0.0, 1.0, 0.0, 0.0,
			0.0, 0.0, 1.0, 0.0,
			0.0, 0.0, 0.0, 1.0
		),
		Orientation::Deg180 => Matrix4::new(
			-1.0, 0.0, 0.0, 0.0,
			0.0, -1.0, 0.0, 0.0,
			0.0, 0.0, 1.0, 0.0,
			0.0, 0.0, 0.0, 1.0
		),
		Orientation::Deg180HorFlip => Matrix4::new(
			1.0, 0.0, 0.0, 0.0,
			0.0, -1.0, 0.0, 0.0,
			0.0, 0.0, 1.0, 0.0,
			0.0, 0.0, 0.0, 1.0
		),
		Orientation::Deg90 => Matrix4::new(
			0.0, -1.0, 0.0, 0.0,
			1.0, 0.0, 0.0, 0.0,
			0.0, 0.0, 1.0, 0.0,
			0.0, 0.0, 0.0, 1.0
		),
		Orientation::Deg90VerFlip => Matrix4::new(
			0.0, -1.0, 0.0, 0.0,
			-1.0, 0.0, 0.0, 0.0,
			0.0, 0.0, 1.0, 0.0,
			0.0, 0.0, 0.0, 1.0
		),
		Orientation::Deg270 => Matrix4::new(
			0.0, 1.0, 0.0, 0.0,
			-1.0, 0.0, 0.0, 0.0,
			0.0, 0.0, 1.0, 0.0,
			0.0, 0.0, 0.0, 1.0
		),
		Orientation::Deg270VerFlip => Matrix4::new(
			0.0, 1.0, 0.0, 0.0,
			1.0, 0.0, 0.0, 0.0,
			0.0, 0.0, 1.0, 0.0,
			0.0, 0.0, 0.0, 1.0
		),
	};
	result
}

struct PictureWidgetData {
	placement: WidgetPlacement,
	drawn_bounds: LogicalRect,
	prev_draw_size: LogicalVector,
	visible: bool,
	render_validity: RenderValidity,

	click: bool,
	hover: bool,

	configuration: Rc<RefCell<Configuration>>,
	cache: Arc<Mutex<Cache>>,
	playback_manager: PlaybackManager,
	// It's an option to allow manual destruction.
	clipboard_handler: Option<ClipboardHandler>,
	clipboard_request_was_pending: bool,
	/// Outcome of the trash operation running on a worker thread, if one is in flight.
	pending_delete: Option<mpsc::Receiver<bool>>,
	program: Program,
	bright_shade: f32,
	gamma: f32,
	gamma_feedback_until: Option<Instant>,
	/// Size of an image texel in physical display pixels
	img_texel_size: f32,
	scaling: ScalingMode,
	img_pos: LogicalVector,
	antialiasing: Antialias,

	hor_pan_input: MovementDir,
	ver_pan_input: MovementDir,
	zoom_input: MovementDir,
	/// The velocity of horizontal panning
	hor_pan_vel: f32,
	/// The velocity of vertical panning
	ver_pan_vel: f32,
	/// The velocity of zooming
	zoom_vel: f32,
	/// Horizontal scroll collected since the last image step.
	hor_scroll_accum: f32,

	last_click_time: Instant,
	last_mouse_pos: LogicalVector,
	panning_2d: bool,
	panning_vert: bool,
	panning_hor: bool,
	hover_state: HoverState,

	first_draw: bool,
	last_cam_move_time: Instant,
	#[cfg(feature = "benchmark")]
	benchmark_load_started: Option<Instant>,
	next_update: NextUpdate,
	bottom_bar: Rc<BottomBar>,
	thumbnails: Option<Rc<ThumbnailStripe>>,
	thumbnail_mode: StripeMode,
	left_to_pan_hint: Rc<HelpScreen>,
	copy_notifications: CopyNotifications,
	window: Weak<Window>,
	/// Last title handed to the window, so an identical one is not set (and redrawn) again.
	last_title: String,
	/// Last visibility pushed to the thumbnail strip.
	stripe_visible: Option<bool>,
	/// Zoom (`img_texel_size`) the last time it was looked at, and since when it has been that.
	svg_zoom: (f32, Instant),
	/// The SVG texture a sharper render was asked for, and its file. Cleared when the texture
	/// changes.
	svg_rerender: Option<(AnimationFrameTexture, PathBuf)>,
}
impl WidgetData for PictureWidgetData {
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
impl PictureWidgetData {
	fn fit_image_to_panel(&mut self, dpi_scale: f32, stretch: bool) {
		let size = self.drawn_bounds.size.vec;
		if size.x <= 0.0 || size.y <= 0.0 {
			return;
		}
		if let Some(texture) = self.get_texture() {
			let panel_aspect = size.x / size.y;
			let (img_phys_w, img_pyhs_h) = {
				let (w, h) = texture.oriented_dimensions();
				(w as f32, h as f32)
			};
			let img_aspect = img_phys_w / img_pyhs_h;

			let texel_size_to_fit_width = size.x / img_phys_w;
			let img_texel_size = if img_aspect > panel_aspect {
				// The image is relatively wider than the panel
				texel_size_to_fit_width
			} else {
				texel_size_to_fit_width * (img_aspect / panel_aspect)
			};
			let widget_phys_size = size * dpi_scale;
			let fits_in_widget =
				widget_phys_size.x >= img_phys_w && widget_phys_size.y >= img_pyhs_h;
			self.img_pos = LogicalVector::new(size.x * 0.5, size.y * 0.5);
			// An SVG is scaled to the panel like a stretched picture: it has no size of its own
			// to keep, and a bigger panel gets a sharper render of it (`update_svg_resolution`).
			if fits_in_widget && !stretch && !texture.svg {
				self.img_texel_size = 1.0;
			} else {
				self.img_texel_size = img_texel_size * dpi_scale;
			}
			if stretch {
				self.scaling = ScalingMode::FitStretch;
			} else {
				self.scaling = ScalingMode::FitMin;
			}
		}
	}

	fn zoom_image(&mut self, anchor: LogicalVector, delta: f32) {
		if self.img_texel_size <= 0.0 {
			// Nothing has been fitted yet, so there is no scale to zoom from.
			return;
		}
		let image_texel_size = zoomed_texel_size(self.img_texel_size, delta);
		self.img_pos = zoomed_img_pos(self.img_pos, anchor, image_texel_size / self.img_texel_size);
		self.img_texel_size = image_texel_size;
		self.scaling = ScalingMode::Fixed;
		self.update_scaling_buttons();
		self.render_validity.invalidate();
	}

	/// Keeps SVGs sharp. Tells the cache what area SVGs are fitted to when they are loaded,
	/// and, once a zoom has settled, asks for a sharper render of the shown SVG if it is
	/// magnified (`sharper_svg_size`). The old render stays on screen until the new one is
	/// ready. Returns when this has to run again, if it is waiting for the zoom to settle or
	/// for the loader.
	fn update_svg_resolution(&mut self, dpi_scale: f32, now: Instant) -> Option<Instant> {
		let view = self.drawn_bounds.size.vec * dpi_scale;
		if view.x >= 1.0 && view.y >= 1.0 {
			self.playback_manager.set_svg_view(view.x.round() as u32, view.y.round() as u32);
		}
		if (self.img_texel_size - self.svg_zoom.0).abs() > f32::EPSILON {
			self.svg_zoom = (self.img_texel_size, now);
		}
		let texture = self.get_texture().filter(|texture| texture.svg)?;
		// A render that was asked for and failed isn't asked for again.
		if let Some((asked, _)) = &self.svg_rerender
			&& Rc::ptr_eq(&asked.tex_grid, &texture.tex_grid)
		{
			return None;
		}
		let fit = sharper_svg_size((texture.w, texture.h), self.img_texel_size)?;
		let settled = self.svg_zoom.1 + SVG_SETTLE;
		if now < settled {
			return Some(settled);
		}
		let LoadedImgPath::Loaded(path) = self.playback_manager.shown_file_path().clone() else {
			return None;
		};
		if self.playback_manager.rerender_svg(fit) {
			self.svg_rerender = Some((texture, path));
			None
		} else {
			Some(now + SVG_SETTLE)
		}
	}

	/// The texture changed. If it is the sharper render of the SVG asked for, the zoom is
	/// scaled with the texture so that the picture keeps its size.
	fn adopt_rerendered_svg(
		&mut self,
		prev: Option<&AnimationFrameTexture>,
		new: Option<&AnimationFrameTexture>,
	) {
		let Some((asked, path)) = self.svg_rerender.take() else {
			return;
		};
		let (Some(prev), Some(new)) = (prev, new) else {
			return;
		};
		let same_file = matches!(
			self.playback_manager.shown_file_path(),
			LoadedImgPath::Loaded(shown) if *shown == path
		);
		if same_file && new.svg && Rc::ptr_eq(&asked.tex_grid, &prev.tex_grid) {
			self.img_texel_size *= prev.w as f32 / new.w as f32;
			self.update_scaling_buttons();
		}
	}

	fn update_image_transform(&mut self, dpi_scale: f32) {
		match self.scaling {
			ScalingMode::Fixed => {
				let center_offset = (self.drawn_bounds.size - self.prev_draw_size) * 0.5f32;
				self.img_pos += center_offset;
				self.apply_img_bounds(dpi_scale);
			}
			ScalingMode::FitStretch => {
				self.fit_image_to_panel(dpi_scale, true);
			}
			ScalingMode::FitMin => {
				self.fit_image_to_panel(dpi_scale, false);
			}
		}
		self.prev_draw_size = self.drawn_bounds.size;
	}

	fn apply_camera_movement(&mut self) {
		fn animate_value(v: &mut f32, dir: f32, dt: f32, next_update: &mut NextUpdate) {
			#[allow(clippy::float_cmp)]
			if v.signum() != dir {
				*v = 0.0;
			}
			*v += dir * dt * (2.0 / (v.abs() + 1.0));
			*next_update = NextUpdate::Soonest;
		}

		let now = Instant::now();
		let dt_sec = now.duration_since(self.last_cam_move_time).as_secs_f32();
		self.last_cam_move_time = now;

		match self.hor_pan_input {
			MovementDir::None => self.hor_pan_vel = 0.0,
			MovementDir::Positive => {
				animate_value(&mut self.hor_pan_vel, 1.0, dt_sec, &mut self.next_update)
			}
			MovementDir::Negative => {
				animate_value(&mut self.hor_pan_vel, -1.0, dt_sec, &mut self.next_update)
			}
		}
		match self.ver_pan_input {
			MovementDir::None => self.ver_pan_vel = 0.0,
			MovementDir::Positive => {
				animate_value(&mut self.ver_pan_vel, 1.0, dt_sec, &mut self.next_update)
			}
			MovementDir::Negative => {
				animate_value(&mut self.ver_pan_vel, -1.0, dt_sec, &mut self.next_update)
			}
		}
		match self.zoom_input {
			MovementDir::None => self.zoom_vel = 0.0,
			MovementDir::Positive => {
				animate_value(&mut self.zoom_vel, 1.0, dt_sec, &mut self.next_update)
			}
			MovementDir::Negative => {
				animate_value(&mut self.zoom_vel, -1.0, dt_sec, &mut self.next_update)
			}
		}

		if self.zoom_input.moving() {
			let bounds_size = self.drawn_bounds.size.vec;
			let anchor = LogicalVector::new(bounds_size.x * 0.5, bounds_size.y * 0.5);
			self.zoom_image(anchor, self.zoom_vel * dt_sec);
		}
		if self.hor_pan_input.moving() || self.ver_pan_input.moving() {
			// `img_pos` is in logical pixels so the speed must not depend on the DPI scale.
			let panning_speed = 400.0;
			let pos_delta = Vector2::new(self.hor_pan_vel, self.ver_pan_vel) * dt_sec;
			self.scaling = ScalingMode::Fixed;
			self.update_scaling_buttons();
			self.img_pos.vec += panning_speed * pos_delta;
		}
	}

	fn camera_movement_will_start(&mut self) {
		// If there hasn't been any movement in a while, then reset the last update time
		// to avoid large jumps at the beggining of a move when the delta would be large.
		if !self.hor_pan_input.moving() && !self.ver_pan_input.moving() && !self.zoom_input.moving()
		{
			self.last_cam_move_time = Instant::now();
		}
	}

	fn set_window_title_filename(
		&mut self,
		window: &Window,
		playback_state: PlaybackState,
		file_path: &LoadedImgPath,
	) {
		let playback = match playback_state {
			PlaybackState::Forward => " : Playing",
			PlaybackState::Present => " : Presenting",
			PlaybackState::RandomPresent => " : Presenting Shuffled",
			PlaybackState::Paused => "",
		};

		let config = self.configuration.borrow();
		let title_config = config.title.clone().unwrap_or_default();

		let name = match file_path {
			LoadedImgPath::NotYetLoaded => "[ none ]".into(),
			LoadedImgPath::ErrLoading(path) => {
				format!("[ FAILED TO OPEN ] {}", title_config.format_file_path(path)).into()
			}
			LoadedImgPath::Loaded(path) => title_config.format_file_path(path),
		};
		let now = Instant::now();
		let gamma_feedback = match self.gamma_feedback_until {
			Some(until) if now < until => true,
			Some(_) => {
				self.gamma_feedback_until = None;
				false
			}
			None => false,
		};
		let title = if gamma_feedback {
			format!(
				"{}{} : Gamma {:.2}{}",
				name,
				playback,
				self.gamma,
				title_config.format_program_name()
			)
		} else {
			format!("{}{}{}", name, playback, title_config.format_program_name())
		};
		if title != self.last_title {
			self.last_title.clone_from(&title);
			window.set_title(title);
		}
	}

	fn get_texture(&self) -> Option<AnimationFrameTexture> {
		self.playback_manager.image_texture()
	}

	pub fn set_img_size_to_orig(&mut self) {
		self.img_texel_size = 1.0;
		self.scaling = ScalingMode::Fixed;
		self.update_scaling_buttons();
		self.render_validity.invalidate();
	}

	pub fn set_img_size_to_fit(&mut self, stretch: bool) {
		{
			let mut cache = self.cache.lock().unwrap();
			cache.image.fit_stretches = stretch;
		}
		self.scaling = if stretch { ScalingMode::FitStretch } else { ScalingMode::FitMin };
		self.update_scaling_buttons();
		self.render_validity.invalidate();
	}

	pub fn toggle_antialias(&mut self) {
		let aa = match self.antialiasing {
			Antialias::Auto if self.img_texel_size < AA_TEXEL_SIZE_THRESHOLD => Antialias::Never,
			Antialias::Auto | Antialias::Never => Antialias::Always,
			Antialias::Always => Antialias::Never,
		};
		self.antialiasing = aa;
		self.cache.lock().unwrap().image.antialiasing = aa;
		self.render_validity.invalidate();
	}

	pub fn set_automatic_antialias(&mut self) {
		self.antialiasing = Antialias::Auto;
		self.cache.lock().unwrap().image.antialiasing = Antialias::Auto;
		self.render_validity.invalidate();
	}

	/// Ensures that the image is within the widget, or at least touches an edge of the widget
	fn apply_img_bounds(&mut self, dpi_scale: f32) {
		if let Some(texture) = self.get_texture() {
			let (img_phys_w, img_phys_h) = {
				let (w, h) = texture.oriented_dimensions();
				(w as f32 * self.img_texel_size, h as f32 * self.img_texel_size)
			};
			let img_w = img_phys_w / dpi_scale;
			let img_h = img_phys_h / dpi_scale;

			let widget_size = self.drawn_bounds.size.vec;
			let img_pos = self.img_pos.vec;

			if img_pos.x < -img_w / 2.0 {
				self.img_pos.vec.x = -img_w / 2.0;
			}
			if img_pos.y < -img_h / 2.0 {
				self.img_pos.vec.y = -img_h / 2.0;
			}

			if img_pos.x > widget_size.x + img_w / 2.0 {
				self.img_pos.vec.x = (widget_size.x + img_w / 2.0).ceil();
			}
			if img_pos.y > widget_size.y + img_h / 2.0 {
				self.img_pos.vec.y = (widget_size.y + img_h / 2.0).ceil();
			}
		}
	}
	fn pointer_over_bottom_bar(&self, point: LogicalVector) -> bool {
		let bottom = self.drawn_bounds.pos.vec.y + self.drawn_bounds.size.vec.y;
		self.bottom_bar.is_bar_visible() && point.vec.y >= bottom - BOTTOM_BAR_HEIGHT
	}

	fn pointer_over_thumbnails(&self, point: LogicalVector) -> bool {
		self.thumbnails.as_ref().is_some_and(|stripe| stripe.contains_point(point))
	}

	fn pointer_over_overlay(&self, point: LogicalVector) -> bool {
		self.pointer_over_bottom_bar(point) || self.pointer_over_thumbnails(point)
	}

	fn set_gamma(&mut self, gamma: f32, show_feedback: bool) {
		self.gamma = clamp_gamma(gamma);
		self.bottom_bar.set_gamma_slider_value(self.gamma);
		if show_feedback {
			self.gamma_feedback_until = Some(Instant::now() + Duration::from_secs(1));
		}
		self.render_validity.invalidate();
	}

	fn reset_gamma_for_navigation(&mut self) {
		let reset = self
			.configuration
			.borrow()
			.image
			.as_ref()
			.and_then(|section| section.reset_gamma_on_navigation)
			.unwrap_or(false);
		if reset {
			let gamma = self
				.configuration
				.borrow()
				.image
				.as_ref()
				.and_then(|section| section.gamma)
				.unwrap_or(DEFAULT_GAMMA);
			self.set_gamma(gamma, false);
		}
	}

	fn adjust_gamma(&mut self, direction: i32) {
		self.set_gamma(self.gamma + direction as f32 * 0.1, true);
	}

	fn reset_gamma(&mut self) {
		self.set_gamma(DEFAULT_GAMMA, true);
	}

	fn update_scaling_buttons(&mut self) {
		self.bottom_bar.update_scaling_buttons(self.scaling, self.img_texel_size);
	}

	/// Moves the shown file to the trash on a worker thread, `poll_pending_delete` finishes the job.
	fn delete_shown_image(&mut self) {
		if self.pending_delete.is_some() {
			return;
		}
		let LoadedImgPath::Loaded(path) = self.playback_manager.shown_file_path().clone() else {
			return;
		};
		let (sender, receiver) = mpsc::channel();
		std::thread::spawn(move || {
			let result = trash::delete(&path);
			if let Err(error) = &result {
				log::error!("Error while moving file '{path:?}' to trash: {error:?}");
			}
			// The receiver is only gone when the widget is, then nobody needs the result.
			let _ = sender.send(result.is_ok());
		});
		self.pending_delete = Some(receiver);
	}

	/// Picks up the result of `delete_shown_image`: refreshes the folder (which advances to the
	/// next image) on success and shows the failure notification otherwise.
	fn poll_pending_delete(&mut self) {
		let Some(receiver) = &self.pending_delete else {
			return;
		};
		let succeeded = match receiver.try_recv() {
			Ok(deleted) => deleted,
			Err(mpsc::TryRecvError::Empty) => {
				let next = Instant::now() + Duration::from_millis(50);
				self.next_update = self.next_update.aggregate(NextUpdate::WaitUntil(next));
				return;
			}
			Err(mpsc::TryRecvError::Disconnected) => {
				log::error!("The trash thread stopped unexpectedly.");
				false
			}
		};
		self.pending_delete = None;
		if succeeded {
			self.playback_manager.update_directory();
		} else {
			self.copy_notifications.set_delete_failed();
		}
		self.render_validity.invalidate();
	}

	fn copy_shown_image(&mut self) {
		if let LoadedImgPath::Loaded(path) = self.playback_manager.shown_file_path().clone()
			&& let Some(clipboard_handler) = &mut self.clipboard_handler
		{
			clipboard_handler.request_copy(path);
			self.copy_notifications.set_started();
			self.clipboard_request_was_pending = true;
		}
	}
}

pub struct PictureWidget {
	data: RefCell<PictureWidgetData>,
}
impl PictureWidget {
	pub fn new(
		display: &Display,
		window: &Rc<Window>,
		bottom_bar: Rc<BottomBar>,
		left_to_pan_hint: Rc<HelpScreen>,
		copy_notifications: CopyNotifications,
		configuration: Rc<RefCell<Configuration>>,
		cache: Arc<Mutex<Cache>>,
	) -> PictureWidget {
		let program = gelatin::shaders::shader_from_source(
			display,
			ShaderDescriptor {
				vertex_shader: shaders::VERTEX_140,
				fragment_shader: shaders::FRAGMENT_140,
				outputs_srgb: false,
			},
		)
		.unwrap();

		let scaling;
		{
			let cache = cache.lock().unwrap();
			if cache.image.fit_stretches {
				scaling = ScalingMode::FitStretch;
			} else {
				scaling = ScalingMode::FitMin;
			}
		}

		let antialiasing = configuration
			.borrow()
			.image
			.as_ref()
			.and_then(|s| s.antialiasing.clone())
			.unwrap_or_else(|| "auto".into());

		let antialiasing = match antialiasing.as_str() {
			"auto" => Antialias::Auto,
			"always" => Antialias::Always,
			"never" => Antialias::Never,
			"previous" => cache.lock().unwrap().image.antialiasing,
			val => {
				eprintln!("Illegal configuration value {:?} for antialiasing!", val);
				eprintln!(r#"Allowed values are "auto", "always", "never" and "previous"."#);
				Antialias::default()
			}
		};

		let gamma = configuration
			.borrow()
			.image
			.as_ref()
			.and_then(|section| section.gamma)
			.map(clamp_gamma)
			.unwrap_or(DEFAULT_GAMMA);

		let mut data = PictureWidgetData {
			placement: Default::default(),
			drawn_bounds: Default::default(),
			visible: true,
			prev_draw_size: Default::default(),
			click: false,
			hover: false,
			configuration,
			cache,
			playback_manager: PlaybackManager::new(),
			clipboard_handler: Some(ClipboardHandler::new()),
			clipboard_request_was_pending: false,
			pending_delete: None,
			render_validity: Default::default(),

			program,
			bright_shade: 0.95,
			gamma,
			gamma_feedback_until: None,
			img_texel_size: 0.0,
			scaling,
			img_pos: Default::default(),
			antialiasing,
			hor_pan_input: MovementDir::None,
			ver_pan_input: MovementDir::None,
			zoom_input: MovementDir::None,
			hor_pan_vel: 0.0,
			ver_pan_vel: 0.0,
			zoom_vel: 0.0,
			hor_scroll_accum: 0.0,
			last_click_time: long_ago(),
			last_mouse_pos: Default::default(),
			panning_2d: false,
			panning_vert: false,
			panning_hor: false,
			hover_state: HoverState::None,
			last_cam_move_time: Instant::now(),
			#[cfg(feature = "benchmark")]
			benchmark_load_started: None,
			first_draw: true,
			next_update: NextUpdate::Latest,
			bottom_bar,
			thumbnails: None,
			thumbnail_mode: StripeMode::Off,
			left_to_pan_hint,
			copy_notifications,
			window: Rc::downgrade(window),
			last_title: String::new(),
			stripe_visible: None,
			svg_zoom: (0.0, Instant::now()),
			svg_rerender: None,
		};
		data.bottom_bar.set_gamma_slider_value(data.gamma);
		data.update_scaling_buttons();
		PictureWidget { data: RefCell::new(data) }
	}
	add_common_widget_functions!(data);

	/// Attaches the folder strip; the widget then keeps it fed with folder state and paths.
	pub fn set_thumbnail_stripe(&self, stripe: Rc<ThumbnailStripe>, mode: StripeMode) {
		let mut data = self.data.borrow_mut();
		data.thumbnails = Some(stripe);
		data.stripe_visible = None;
		data.thumbnail_mode = mode;
		data.render_validity.invalidate();
	}

	pub fn thumbnail_mode(&self) -> StripeMode {
		self.data.borrow().thumbnail_mode
	}

	pub fn set_thumbnail_mode(&self, mode: StripeMode) {
		let mut data = self.data.borrow_mut();
		data.thumbnail_mode = mode;
		let pinned = mode == StripeMode::Pinned;
		if let Some(stripe) = data.thumbnails.clone() {
			stripe.set_visible(pinned);
			data.stripe_visible = Some(pinned);
		}
		data.render_validity.invalidate();
	}

	/// Cycles Off -> auto-hide -> pinned and remembers the choice.
	pub fn cycle_thumbnail_mode(&self) {
		let mode = self.thumbnail_mode().next();
		log::debug!("Thumbnail strip mode: {mode:?}");
		self.set_thumbnail_mode(mode);
		let data = self.data.borrow();
		data.cache.lock().unwrap().window.thumbnails = mode.as_str().to_owned();
	}

	pub fn set_bright_shade(&self, shade: f32) {
		let mut borrowed = self.data.borrow_mut();
		borrowed.bright_shade = shade;
		borrowed.render_validity.invalidate();
	}
	pub fn set_img_size_to_orig(&self) {
		let mut borrowed = self.data.borrow_mut();
		borrowed.set_img_size_to_orig();
	}

	pub fn set_gamma(&self, gamma: f32) {
		let mut borrowed = self.data.borrow_mut();
		borrowed.set_gamma(gamma, true);
	}

	pub fn set_gamma_from_slider(&self, position: u32) {
		let mut borrowed = self.data.borrow_mut();
		borrowed.set_gamma(gamma_from_slider_position(position), true);
	}
	pub fn reset_gamma(&self) {
		self.data.borrow_mut().reset_gamma();
	}

	pub fn set_img_size_to_fit(&self, stretch: bool) {
		let mut borrowed = self.data.borrow_mut();
		borrowed.set_img_size_to_fit(stretch);
	}

	#[cfg(feature = "benchmark")]
	fn benchmark_load_started(data: &mut PictureWidgetData) {
		data.benchmark_load_started = Some(Instant::now());
	}
	pub fn jump_to_index(&self, index: u32) {
		let mut borrowed = self.data.borrow_mut();
		borrowed.reset_gamma_for_navigation();
		#[cfg(feature = "benchmark")]
		Self::benchmark_load_started(&mut borrowed);
		borrowed.playback_manager.request_load(LoadRequest::LoadAtIndex(index as usize));
		borrowed.render_validity.invalidate();
	}

	pub fn jump_to_path<P: Into<PathBuf>>(&self, path: P) {
		let mut borrowed = self.data.borrow_mut();
		borrowed.reset_gamma_for_navigation();
		#[cfg(feature = "benchmark")]
		Self::benchmark_load_started(&mut borrowed);
		borrowed.playback_manager.request_load(LoadRequest::FilePath(path.into()));
		borrowed.render_validity.invalidate();
	}

	pub fn perform_action(&self, action_name: &str) {
		if action_name == "toggle_thumbnails" {
			self.cycle_thumbnail_mode();
			return;
		}
		let mut data = self.data.borrow_mut();
		match action_name {
			"toggle_fullscreen" => {
				if let Some(window) = data.window.upgrade() {
					let fullscreen = !window.fullscreen();
					window.set_fullscreen(fullscreen);
					data.bottom_bar.set_fullscreen(fullscreen);
				}
			}
			"img_prev" | "img_next" => {
				data.reset_gamma_for_navigation();
				#[cfg(feature = "benchmark")]
				Self::benchmark_load_started(&mut data);
				let request = if action_name == "img_prev" {
					LoadRequest::LoadPrevious
				} else {
					LoadRequest::LoadNext
				};
				data.playback_manager.request_load(request);
				data.render_validity.invalidate();
			}
			"gamma_decrease" => data.adjust_gamma(-1),
			"gamma_increase" => data.adjust_gamma(1),
			"gamma_reset" => data.reset_gamma(),
			"img_fit" => data.set_img_size_to_fit(true),
			"img_fit_best" => data.set_img_size_to_fit(false),
			"img_orig" => data.set_img_size_to_orig(),
			"toggle_antialias" => data.toggle_antialias(),
			"automatic_antialias" => data.set_automatic_antialias(),
			"play_anim" => match data.playback_manager.playback_state() {
				PlaybackState::Forward => data.playback_manager.pause_playback(),
				_ => data.playback_manager.start_playback_forward(),
			},
			"play_present" => {
				match data.playback_manager.playback_state() {
					PlaybackState::Present => data.playback_manager.pause_playback(),
					_ => data.playback_manager.start_presentation(),
				}
				data.render_validity.invalidate();
			}
			"play_present_rnd" => {
				match data.playback_manager.playback_state() {
					PlaybackState::RandomPresent => data.playback_manager.pause_playback(),
					_ => data.playback_manager.start_random_presentation(),
				}
				data.render_validity.invalidate();
			}
			"img_del" => data.delete_shown_image(),
			"img_copy" => data.copy_shown_image(),
			_ => log::warn!("Ignoring unknown application action '{action_name}'"),
		}
	}

	/// Runs the actions and custom commands bound to the key. Returns whether any binding fired.
	fn handle_key_input(&self, input_key: &str, modifiers: ModifiersState) -> bool {
		let mut borrowed = self.data.borrow_mut();
		let mut fired = false;
		macro_rules! triggered {
			($action_name:ident) => {{
				let triggered =
					action_triggered(&borrowed.configuration, $action_name, input_key, modifiers);
				fired |= triggered;
				triggered
			}};
		}
		if triggered!(TOGGLE_FULLSCREEN_NAME)
			&& let Some(window) = borrowed.window.upgrade()
		{
			let fullscreen = !window.fullscreen();
			window.set_fullscreen(fullscreen);
			borrowed.bottom_bar.set_fullscreen(fullscreen);
		}
		if triggered!(ESCAPE_NAME)
			&& let Some(window) = borrowed.window.upgrade()
		{
			if window.fullscreen() {
				window.set_fullscreen(false);
				borrowed.bottom_bar.set_fullscreen(false);
			} else {
				request_exit();
			}
		}
		if triggered!(PLAY_ANIM_NAME) {
			match borrowed.playback_manager.playback_state() {
				PlaybackState::Forward => borrowed.playback_manager.pause_playback(),
				_ => borrowed.playback_manager.start_playback_forward(),
			}
		}
		if triggered!(IMG_PREV_NAME) {
			borrowed.reset_gamma_for_navigation();
			#[cfg(feature = "benchmark")]
			Self::benchmark_load_started(&mut borrowed);
			borrowed.playback_manager.request_load(LoadRequest::LoadPrevious);
			borrowed.render_validity.invalidate();
		}
		if triggered!(IMG_NEXT_NAME) {
			borrowed.reset_gamma_for_navigation();
			#[cfg(feature = "benchmark")]
			Self::benchmark_load_started(&mut borrowed);
			borrowed.playback_manager.request_load(LoadRequest::LoadNext);
			borrowed.render_validity.invalidate();
		}
		if triggered!(GAMMA_DECREASE_NAME) {
			borrowed.adjust_gamma(-1);
		}
		if triggered!(GAMMA_INCREASE_NAME) {
			borrowed.adjust_gamma(1);
		}
		if triggered!(GAMMA_RESET_NAME) {
			borrowed.reset_gamma();
		}
		if triggered!(IMG_FIT_NAME) {
			borrowed.set_img_size_to_fit(true);
		}
		if triggered!(IMG_FIT_BEST_NAME) {
			borrowed.set_img_size_to_fit(false);
		}
		if triggered!(IMG_ORIG_NAME) {
			borrowed.set_img_size_to_orig();
		}
		if triggered!(TOGGLE_ANTIALIAS_NAME) {
			borrowed.toggle_antialias();
		}
		if triggered!(SET_AUTOMATIC_ANTIALIAS_NAME) {
			borrowed.set_automatic_antialias();
		}
		if triggered!(PLAY_PRESENT_NAME) {
			match borrowed.playback_manager.playback_state() {
				PlaybackState::Present => borrowed.playback_manager.pause_playback(),
				_ => borrowed.playback_manager.start_presentation(),
			}
			borrowed.render_validity.invalidate();
		}
		if triggered!(PLAY_PRESENT_RND_NAME) {
			match borrowed.playback_manager.playback_state() {
				PlaybackState::RandomPresent => borrowed.playback_manager.pause_playback(),
				_ => borrowed.playback_manager.start_random_presentation(),
			}
			borrowed.render_validity.invalidate();
		}
		if triggered!(IMG_DEL_NAME) {
			borrowed.delete_shown_image();
		}
		if triggered!(IMG_COPY_NAME) {
			borrowed.copy_shown_image();
		}
		if let LoadedImgPath::Loaded(img_path) = borrowed.playback_manager.shown_file_path() {
			if let Some(folder_path) = img_path.parent() {
				let img_and_folder = (img_path.to_str(), folder_path.to_str());
				if let (Some(img_path), Some(folder_path)) = img_and_folder {
					fired |= execute_triggered_commands(
						borrowed.configuration.clone(),
						input_key,
						modifiers,
						img_path,
						folder_path,
					);
				} else {
					log::error!("Could not convert the image path to utf8. Path: '{:?}'", img_path);
				}
			} else {
				log::error!("Could not get parent folder for the image path {:?}", img_path);
			}
		}
		let cycle_thumbnails = triggered!(TOGGLE_THUMBNAILS_NAME);
		drop(borrowed);
		if cycle_thumbnails {
			self.cycle_thumbnail_mode();
		}
		fired
	}
}

impl Widget for PictureWidget {
	fn before_draw(&self, window: &Window) -> NextUpdate {
		let mut data = self.data.borrow_mut();
		if !data.visible {
			return NextUpdate::Latest;
		}
		if data.first_draw {
			// Don't block on the main thread and
			// wait on the image to be loaded on the first draw,
			// instead let the ui draw itself first and then we can wait.
			data.first_draw = false;
			data.next_update = NextUpdate::Soonest;
			return data.next_update;
		}
		let now = Instant::now();
		let svg_check_at =
			data.update_svg_resolution(window.window_mut().scale_factor() as f32, now);
		let prev_texture = data.playback_manager.image_texture();
		data.next_update = data.playback_manager.update_image(window);
		if let Some(check_at) = svg_check_at {
			data.next_update = data.next_update.aggregate(NextUpdate::WaitUntil(check_at));
		}
		let new_texture = data.playback_manager.image_texture();
		let curr_file_index = data.playback_manager.current_file_index();
		let curr_dir_len = data.playback_manager.current_dir_len();
		if let (Some(curr_file_index), Some(curr_dir_len)) = (curr_file_index, curr_dir_len) {
			data.bottom_bar.slider.set_steps(curr_dir_len as u32, curr_file_index as u32);
		}
		let playback_state = data.playback_manager.playback_state();
		let shown_file_path = data.playback_manager.shown_file_path().clone();
		data.set_window_title_filename(window, playback_state, &shown_file_path);
		let bottom_bar_next_update = data.bottom_bar.before_draw();
		data.next_update = data.next_update.aggregate(bottom_bar_next_update);
		if let Some(gamma_feedback_until) = data.gamma_feedback_until {
			data.next_update =
				data.next_update.aggregate(NextUpdate::WaitUntil(gamma_feedback_until));
		}
		if data.thumbnail_mode != StripeMode::Off
			&& let Some(stripe) = data.thumbnails.clone()
		{
			if let (Some(index), Some(len)) = (curr_file_index, curr_dir_len) {
				let folder = data.playback_manager.path_by_index(index).map(|mut path| {
					path.pop();
					path
				});
				stripe.set_folder_state(index, len, folder.as_deref());
			}
			let bar_visible = data.bottom_bar.is_bar_visible();
			stripe.set_bottom_offset(if bar_visible { BOTTOM_BAR_HEIGHT } else { 0.0 });
			let stripe_visible = match data.thumbnail_mode {
				StripeMode::Pinned => true,
				StripeMode::AutoHide => bar_visible,
				StripeMode::Off => false,
			};
			if data.stripe_visible != Some(stripe_visible) {
				data.stripe_visible = Some(stripe_visible);
				stripe.set_visible(stripe_visible);
			}
			if let Some(wanted) = stripe.take_path_requests() {
				let paths = wanted
					.into_iter()
					.filter_map(|index| {
						data.playback_manager.path_by_index(index).map(|path| (index, path))
					})
					.collect::<Vec<_>>();
				stripe.set_paths(paths);
			}
			if stripe.poll(&window.display_mut()) {
				data.render_validity.invalidate();
			}
		}
		let texture_changed = match (&prev_texture, &new_texture) {
			(None, Some(_)) => true,
			(Some(prev_tex), Some(new_tex)) => !Rc::ptr_eq(&prev_tex.tex_grid, &new_tex.tex_grid),
			_ => false,
		};
		if texture_changed {
			data.adopt_rerendered_svg(prev_texture.as_ref(), new_texture.as_ref());
			data.render_validity.invalidate();
			#[cfg(feature = "benchmark")]
			if let Some(started) = data.benchmark_load_started.take() {
				println!(
					"EMULSION_BENCH image_ready_ms={:.3}",
					started.elapsed().as_secs_f64() * 1000.0
				);
			}
		}
		data.poll_pending_delete();
		if let Some(clipboard_handler) = &data.clipboard_handler {
			let clipboard_result = clipboard_handler.try_get_result();
			let request_pending = clipboard_result.is_none();
			if data.clipboard_request_was_pending != request_pending {
				match clipboard_result {
					Some(succeeded) => data.copy_notifications.set_finished(succeeded),
					None => data.copy_notifications.set_started(),
				}
				data.clipboard_request_was_pending = request_pending;
			} else if request_pending {
				let next_update = now + Duration::from_millis(100);
				data.next_update = data.next_update.aggregate(NextUpdate::WaitUntil(next_update));
			}
		}
		if data.zoom_input.moving() || data.hor_pan_input.moving() || data.ver_pan_input.moving() {
			data.render_validity.invalidate();
			data.next_update = NextUpdate::Soonest;
		}
		let next_copy_noti_update = data.copy_notifications.update();
		data.next_update = data.next_update.aggregate(next_copy_noti_update);
		data.next_update
	}

	fn draw(&self, target: &mut Frame, context: &DrawContext) -> Result<NextUpdate, WidgetError> {
		let texture;
		{
			let mut data = self.data.borrow_mut();
			if !data.visible {
				return Ok(data.next_update);
			}
			data.update_image_transform(context.dpi_scale_factor);
			data.apply_camera_movement();
			texture = data.get_texture();
		}
		if let Some(texture) = texture {
			let data = self.data.borrow();
			draw_tex_grid(data, target, context, texture);
		}
		let borrowed = self.data.borrow();
		Ok(borrowed.next_update)
	}

	fn layout(&self, available_space: LogicalRect) {
		let mut borrowed = self.data.borrow_mut();
		borrowed.default_layout(available_space);
		borrowed.hover = borrowed.drawn_bounds.contains(borrowed.last_mouse_pos);
	}

	fn handle_event(&self, event: &Event) {
		if !self.data.borrow().visible {
			return;
		}
		match event.kind {
			EventKind::MouseMove => {
				let mut borrowed = self.data.borrow_mut();
				if let Some(window) = borrowed.window.upgrade() {
					let window_height = {
						let window_ref = window.window_mut();
						window_ref.inner_size().height as f32 / window_ref.scale_factor() as f32
					};
					borrowed.bottom_bar.handle_pointer_move(event.cursor_pos.vec.y, window_height);
				}
				borrowed.hover = borrowed.drawn_bounds.contains(event.cursor_pos);
				if borrowed.panning_2d || borrowed.panning_hor || borrowed.panning_vert {
					let mut delta = event.cursor_pos - borrowed.last_mouse_pos;
					if !borrowed.panning_2d {
						if !borrowed.panning_hor {
							// only vertical panning
							delta.vec.x = 0.0;
						}
						if !borrowed.panning_vert {
							// only horzontal panning
							delta.vec.y = 0.0;
						}
					}
					borrowed.scaling = ScalingMode::Fixed;
					borrowed.update_scaling_buttons();
					borrowed.img_pos += delta;
					borrowed.render_validity.invalidate();
				}
				borrowed.last_mouse_pos = event.cursor_pos;
			}
			EventKind::MouseButton { state, button, .. } => {
				log::debug!("Mouse button event: {button:?} {state:?}");
				#[cfg(target_os = "macos")]
				if state == ElementState::Pressed
					&& let Some(direction) = macos_side_button_direction(button)
				{
					let mut borrowed = self.data.borrow_mut();
					borrowed.reset_gamma_for_navigation();
					#[cfg(feature = "benchmark")]
					Self::benchmark_load_started(&mut borrowed);
					let request = if direction < 0 {
						LoadRequest::LoadPrevious
					} else {
						LoadRequest::LoadNext
					};
					borrowed.playback_manager.request_load(request);
					borrowed.render_validity.invalidate();
					return;
				}
				match button {
					MouseButton::Left => {
						let mut borrowed = self.data.borrow_mut();
						if state == ElementState::Pressed {
							let over_overlay = borrowed.pointer_over_overlay(event.cursor_pos);
							let in_drag_strip = cfg!(target_os = "macos")
								&& !over_overlay
								&& event.cursor_pos.vec.y <= MACOS_DRAG_STRIP_HEIGHT
								&& borrowed
									.window
									.upgrade()
									.is_some_and(|window| !window.fullscreen());
							if in_drag_strip {
								let window = borrowed.window.upgrade();
								// The macOS drag loop pumps events, so the widget must not stay
								// borrowed while it runs.
								drop(borrowed);
								if let Some(window) = window
									&& let Err(error) = window.drag_window()
								{
									log::warn!("Could not start native window drag: {error}");
								}
								return;
							} else if borrowed.hover && !over_overlay {
								borrowed.click = true;
								borrowed.panning_2d = true;
							}
						} else {
							borrowed.panning_2d = false;
							borrowed.click = false;
							if borrowed.hover
								&& !borrowed.pointer_over_overlay(event.cursor_pos)
								&& (!cfg!(target_os = "macos")
									|| event.cursor_pos.vec.y > MACOS_DRAG_STRIP_HEIGHT)
							{
								let now = Instant::now();
								let is_double_click = now.duration_since(borrowed.last_click_time)
									< Duration::from_millis(250);
								// A double click consumes both clicks, so the third click of a
								// triple click doesn't toggle fullscreen back.
								borrowed.last_click_time =
									if is_double_click { long_ago() } else { now };
								if is_double_click {
									match borrowed.window.upgrade() {
										Some(window) => {
											let fullscreen = !window.fullscreen();
											window.set_fullscreen(fullscreen);
											borrowed.bottom_bar.set_fullscreen(fullscreen);
										}
										None => log::warn!(
											"The window is gone; can't toggle fullscreen"
										),
									}
								}
							}
						}
						borrowed.render_validity.invalidate();
					}
					MouseButton::Middle => {
						let mut borrowed = self.data.borrow_mut();
						if state == ElementState::Pressed
							&& borrowed.hover
							&& !borrowed.pointer_over_overlay(event.cursor_pos)
						{
							// Middle click toggles between 1:1 and the user's fit mode (Fill or
							// Best fit) without changing that preference.
							if borrowed.scaling == ScalingMode::Fixed {
								let stretch = borrowed.cache.lock().unwrap().image.fit_stretches;
								borrowed.set_img_size_to_fit(stretch);
							} else {
								borrowed.set_img_size_to_orig();
							}
						}
					}
					MouseButton::Right => {
						let borrowed = self.data.borrow();
						let pressed = state == ElementState::Pressed;
						borrowed.left_to_pan_hint.set_visible(pressed);
					}

					_ => {}
				}
			}
			EventKind::MouseScroll { delta } => {
				let mut borrowed = self.data.borrow_mut();
				// Over the bottom bar the sliders under the pointer own the wheel.
				if borrowed.pointer_over_bottom_bar(event.cursor_pos) {
					return;
				}
				// A horizontal swipe (MX Master thumb wheel, Logi "Back/Forward" gesture,
				// trackpad two-finger swipe) steps through the folder instead of zooming,
				// and so does any scroll over the thumbnail strip.
				let over_thumbnails = borrowed.pointer_over_thumbnails(event.cursor_pos);
				if over_thumbnails || delta.vec.x.abs() > delta.vec.y.abs() {
					let step =
						if over_thumbnails { delta.vec.x + delta.vec.y } else { delta.vec.x };
					borrowed.hor_scroll_accum += step;
					if let Some(direction) = take_swipe_step(&mut borrowed.hor_scroll_accum) {
						borrowed.reset_gamma_for_navigation();
						#[cfg(feature = "benchmark")]
						Self::benchmark_load_started(&mut borrowed);
						let request = if direction < 0 {
							LoadRequest::LoadPrevious
						} else {
							LoadRequest::LoadNext
						};
						borrowed.playback_manager.request_load(request);
						borrowed.render_validity.invalidate();
					}
					return;
				}
				borrowed.hor_scroll_accum = 0.0;
				let delta = delta.vec.y * 0.375;
				// `img_pos` is relative to this widget while the cursor is in window space.
				let anchor = event.cursor_pos - borrowed.drawn_bounds.pos;
				borrowed.zoom_image(anchor, delta);
			}
			EventKind::KeyInput { ref input } => {
				let key = input.key_without_modifiers();
				let is_pressed = input.state == ElementState::Pressed;
				let input_key_str = virtual_keycode_to_string(&key).to_lowercase();
				let input_key_str = char_to_input_key(&input_key_str);
				if is_pressed {
					let fired = self.handle_key_input(input_key_str.as_str(), event.modifiers);
					// Non-US layouts report a different logical key for the same physical key
					// (Czech `[` reports `ú`), so bindings are matched by position too, but only
					// when the logical key matched nothing: on AZERTY both names can be bound.
					if !fired
						&& let Some(physical) = physical_key_to_string(&input.physical_key)
						&& physical != input_key_str
					{
						self.handle_key_input(physical, event.modifiers);
					}
				}
				// Panning is a special snowflake
				let mut borrowed = self.data.borrow_mut();
				if action_triggered(
					&borrowed.configuration,
					PAN_NAME,
					input_key_str.as_str(),
					event.modifiers,
				) {
					borrowed.panning_2d = is_pressed;
				}
				if action_triggered(
					&borrowed.configuration,
					PAN_VERT_NAME,
					input_key_str.as_str(),
					event.modifiers,
				) {
					borrowed.panning_vert = is_pressed;
				}
				if action_triggered(
					&borrowed.configuration,
					PAN_HOR_NAME,
					input_key_str.as_str(),
					event.modifiers,
				) {
					borrowed.panning_hor = is_pressed;
				}

				macro_rules! movement_trigger {
					($input:expr, $vel:expr, $name:expr, $dir:expr) => {
						if action_triggered(
							&borrowed.configuration,
							$name,
							input_key_str.as_str(),
							event.modifiers,
						) {
							if $input == $dir && !is_pressed {
								$input = MovementDir::None;
								$vel = 0.0;
							}
							if $input != $dir && is_pressed {
								borrowed.camera_movement_will_start();
								$input = $dir;
							}
						}
					};
				}

				movement_trigger!(
					borrowed.zoom_input,
					borrowed.zoom_vel,
					ZOOM_IN_NAME,
					MovementDir::Positive
				);
				movement_trigger!(
					borrowed.zoom_input,
					borrowed.zoom_vel,
					ZOOM_OUT_NAME,
					MovementDir::Negative
				);

				movement_trigger!(
					borrowed.hor_pan_input,
					borrowed.hor_pan_vel,
					PAN_LEFT_NAME,
					MovementDir::Positive
				);
				movement_trigger!(
					borrowed.hor_pan_input,
					borrowed.hor_pan_vel,
					PAN_RIGHT_NAME,
					MovementDir::Negative
				);

				movement_trigger!(
					borrowed.ver_pan_input,
					borrowed.ver_pan_vel,
					PAN_UP_NAME,
					MovementDir::Positive
				);
				movement_trigger!(
					borrowed.ver_pan_input,
					borrowed.ver_pan_vel,
					PAN_DOWN_NAME,
					MovementDir::Negative
				);
			}
			EventKind::DroppedFile(ref path) => {
				let mut borrowed = self.data.borrow_mut();
				borrowed.playback_manager.request_load(LoadRequest::FilePath(path.clone()));
				borrowed.hover_state = HoverState::None;
				borrowed.render_validity.invalidate();
			}
			EventKind::HoveredFile(ref path) => {
				let mut borrowed = self.data.borrow_mut();
				match borrowed.hover_state {
					HoverState::None => {
						let curr_path = borrowed.playback_manager.shown_file_path().clone();
						borrowed.hover_state = HoverState::ItemHovered { prev_path: curr_path };
					}
					HoverState::ItemHovered { .. } => {}
				}
				borrowed.playback_manager.request_load(LoadRequest::FilePath(path.clone()));
				borrowed.render_validity.invalidate();
			}
			EventKind::HoveredFileCancelled => {
				let mut borrowed = self.data.borrow_mut();
				match borrowed.hover_state.clone() {
					HoverState::None => {
						// Suprisingly this does happen sometimes, so let's just ignore this.
					}
					HoverState::ItemHovered { prev_path } => {
						if let LoadedImgPath::Loaded(prev_path) = prev_path {
							borrowed
								.playback_manager
								.request_load(LoadRequest::FilePath(prev_path));
						}
						borrowed.hover_state = HoverState::None;
					}
				}
				borrowed.render_validity.invalidate();
			}
			EventKind::Focused(focused) => {
				if focused {
					let mut borrowed = self.data.borrow_mut();
					borrowed.playback_manager.update_directory();
					borrowed.render_validity.invalidate();
				}
			}
			EventKind::CloseRequested => {
				let mut borrowed = self.data.borrow_mut();
				// Just let it drop.
				borrowed.clipboard_handler.take();
			}
		}
	}

	// No children for a button
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

fn draw_tex_grid(
	data: Ref<PictureWidgetData>,
	target: &mut Frame,
	context: &DrawContext,
	texture: AnimationFrameTexture,
) {
	let size = data.drawn_bounds.size.vec;
	let projection_transform = gelatin::cgmath::ortho(0.0, size.x, size.y, 0.0, -1.0, 1.0);

	let viewport_rect = context.logical_rect_to_viewport(&data.drawn_bounds);
	let image_draw_params =
		gelatin::glium::DrawParameters { viewport: Some(viewport_rect), ..Default::default() };

	let img_phys_w = texture.w as f32;
	let img_phys_h = texture.h as f32;
	let img_height_over_width = img_phys_h / img_phys_w;
	let image_display_width = data.img_texel_size * img_phys_w / context.dpi_scale_factor;
	let image_display_height = image_display_width * img_height_over_width;
	// Model tranform
	let img_pyhs_pos = data.img_pos.vec * context.dpi_scale_factor;
	let img_phys_siz = {
		let img_phys_w = image_display_width * context.dpi_scale_factor;
		let img_phys_h = image_display_height * context.dpi_scale_factor;
		LogicalVector::new(img_phys_w.ceil(), img_phys_h.ceil())
	};
	let img_logical_corner_x =
		(img_pyhs_pos.x - img_phys_siz.vec.x * 0.5).ceil() / context.dpi_scale_factor;
	let img_logical_corner_y =
		(img_pyhs_pos.y - img_phys_siz.vec.y * 0.5).ceil() / context.dpi_scale_factor;

	// This is the display width of the image in logical pixel units
	let img_adjusted_w = img_phys_siz.vec.x / context.dpi_scale_factor;
	// This is the display height of the image in logical pixel units
	let img_adjusted_h = img_phys_siz.vec.y / context.dpi_scale_factor;
	let img_scaling = Matrix4::from_nonuniform_scale(img_adjusted_w, img_adjusted_h, 1.0);
	let orientation;
	{
		let to_center = Matrix4::from_translation(Vector3::new(
			-0.5 * img_adjusted_w,
			-0.5 * img_adjusted_h,
			0.0,
		));
		let orient = orientation_to_matrix(texture.orientation);
		let to_corner = Matrix4::from_translation(Vector3::new(
			0.5 * img_adjusted_w,
			0.5 * img_adjusted_h,
			0.0,
		));
		orientation = to_corner * orient * to_center;
	}
	let img_translation =
		Matrix4::from_translation(Vector3::new(img_logical_corner_x, img_logical_corner_y, 0.0));

	let cell_phy_step = texture.cell_step_size;
	for cell_tex in texture.tex_grid.iter() {
		let (cell_phys_w, cell_phys_h) = cell_tex.tex.dimensions();

		let cell_phy_offset_x = cell_phy_step * cell_tex.col;
		let cell_phy_offset_y = cell_phy_step * cell_tex.row;

		// The grid is constructed so that it is exactly of size (1, 1) and is located at (0, 0)
		// This allows to leave most of the image transformation logic unchanged.
		let cell_scaling = Matrix4::from_nonuniform_scale(
			cell_phys_w as f32 / img_phys_w,
			cell_phys_h as f32 / img_phys_h,
			1.0,
		);
		let cell_translation = Matrix4::from_translation(Vector3::new(
			cell_phy_offset_x as f32 / img_phys_w,
			cell_phy_offset_y as f32 / img_phys_h,
			0.0,
		));

		let transform =
			img_translation * orientation * img_scaling * cell_translation * cell_scaling;
		// Projection tranform
		let transform = projection_transform * transform;

		let sampler = cell_tex
			.tex
			.sampled()
			.minify_filter(gelatin::glium::uniforms::MinifySamplerFilter::LinearMipmapLinear)
			.wrap_function(gelatin::glium::uniforms::SamplerWrapFunction::Clamp);

		let sampler =
			sampler.magnify_filter(magnify_filter(data.antialiasing, data.img_texel_size));

		// building the uniforms
		let lod_level = lod_for_texel_size(data.img_texel_size);
		let uniforms = uniform! {
			matrix: Into::<[[f32; 4]; 4]>::into(transform),
			bright_shade: data.bright_shade,
			gamma: data.gamma,
			tex: sampler,
			lod_level: lod_level,
			to_window_gamut: texture.primaries.to_window_matrix(),
		};
		target
			.draw(
				context.unit_quad_vertices,
				context.unit_quad_indices,
				&data.program,
				&uniforms,
				&image_draw_params,
			)
			.unwrap_or_else(|error| log::error!("Could not draw the image: {error}"));
	}
}

#[cfg(test)]
mod promise_tests {
	use super::*;

	/// The image point (in texels from the image centre) drawn under `cursor` (window space).
	fn texel_under(
		cursor: LogicalVector,
		widget_pos: LogicalVector,
		img_pos: LogicalVector,
		texel_size: f32,
	) -> (f32, f32) {
		let v = (cursor - widget_pos - img_pos).vec / texel_size;
		(v.x, v.y)
	}

	/// Replays what the wheel handler does: the anchor is the cursor relative to the widget.
	fn wheel(
		cursor: LogicalVector,
		widget_pos: LogicalVector,
		img_pos: &mut LogicalVector,
		texel_size: &mut f32,
		wheel_delta: f32,
	) {
		let new_size = zoomed_texel_size(*texel_size, wheel_delta * 0.375);
		*img_pos = zoomed_img_pos(*img_pos, cursor - widget_pos, new_size / *texel_size);
		*texel_size = new_size;
	}

	#[test]
	fn wheel_zoom_keeps_the_point_under_the_cursor_fixed() {
		// A widget that is not at the window origin (e.g. below the macOS drag strip).
		for widget_pos in [LogicalVector::new(0.0, 0.0), LogicalVector::new(120.0, 48.0)] {
			let cursor = LogicalVector::new(400.0, 300.0);
			let mut img_pos = LogicalVector::new(250.0, 180.0);
			let mut texel_size = 0.8;
			let before = texel_under(cursor, widget_pos, img_pos, texel_size);
			for wheel_delta in [1.0, 2.0, 3.0, -1.0, -4.0, -5.0, 1.0, 1.0, 8.0] {
				let old = texel_size;
				wheel(cursor, widget_pos, &mut img_pos, &mut texel_size, wheel_delta);
				assert_ne!(texel_size, old, "zoomed");
				let after = texel_under(cursor, widget_pos, img_pos, texel_size);
				assert!((after.0 - before.0).abs() < 1e-2, "{after:?} vs {before:?}");
				assert!((after.1 - before.1).abs() < 1e-2, "{after:?} vs {before:?}");
			}
		}
	}

	#[test]
	fn zooming_in_moves_the_image_away_from_the_cursor_and_out_towards_it() {
		let anchor = LogicalVector::new(100.0, 100.0);
		let img_pos = LogicalVector::new(300.0, 200.0);
		let zoomed_in = zoomed_img_pos(img_pos, anchor, 2.0);
		assert_eq!((zoomed_in.vec.x, zoomed_in.vec.y), (500.0, 300.0));
		let zoomed_out = zoomed_img_pos(img_pos, anchor, 0.5);
		assert_eq!((zoomed_out.vec.x, zoomed_out.vec.y), (200.0, 150.0));
		// Zooming about the image centre doesn't move it.
		let same = zoomed_img_pos(img_pos, img_pos, 3.0);
		assert_eq!((same.vec.x, same.vec.y), (300.0, 200.0));
	}

	#[test]
	fn zoom_snaps_to_one_to_one_and_respects_limits() {
		assert_eq!(zoomed_texel_size(0.995, 0.0), 1.0);
		assert_eq!(zoomed_texel_size(MAX_ZOOM_FACTOR, 5.0), MAX_ZOOM_FACTOR);
		assert_eq!(zoomed_texel_size(MIN_ZOOM_FACTOR, -5.0), MIN_ZOOM_FACTOR);
		assert!(zoomed_texel_size(1.0, 0.5) > 1.0 && zoomed_texel_size(1.0, -0.5) < 1.0);
	}

	#[test]
	fn auto_antialiasing_shows_real_pixels_from_the_threshold_on() {
		let auto = |texel_size| magnify_filter(Antialias::Auto, texel_size);
		assert_eq!(auto(AA_TEXEL_SIZE_THRESHOLD), MagnifySamplerFilter::Nearest);
		assert_eq!(auto(4.0), MagnifySamplerFilter::Nearest);
		assert_eq!(auto(32.0), MagnifySamplerFilter::Nearest);
		assert_eq!(auto(3.99), MagnifySamplerFilter::Linear);
		assert_eq!(auto(1.0), MagnifySamplerFilter::Linear);
		assert_eq!(auto(0.25), MagnifySamplerFilter::Linear);
		// The explicit modes ignore the scale.
		assert_eq!(magnify_filter(Antialias::Never, 0.5), MagnifySamplerFilter::Nearest);
		assert_eq!(magnify_filter(Antialias::Always, 64.0), MagnifySamplerFilter::Linear);
	}

	/// Magnified pixel art is sampled from the full-resolution level only: the mip level
	/// can't blur it, and the zoom steps up to the nearest threshold never leave level 0.
	#[test]
	fn magnified_pixel_art_is_never_sampled_from_a_blurred_mip() {
		let mut texel_size = 1.0_f32;
		while texel_size <= MAX_ZOOM_FACTOR {
			assert_eq!(lod_for_texel_size(texel_size), 0.0, "texel size {texel_size}");
			texel_size *= 1.37;
		}
		assert!(lod_for_texel_size(0.99) > 0.0);
	}
}

#[cfg(test)]
mod lod_tests {
	use super::*;

	#[test]
	fn lod_is_zero_when_magnified_or_one_to_one() {
		assert_eq!(lod_for_texel_size(4.0), 0.0);
		assert_eq!(lod_for_texel_size(1.0), 0.0);
	}

	#[test]
	fn lod_grows_continuously_when_minified() {
		assert_eq!(lod_for_texel_size(0.5), 1.0);
		assert_eq!(lod_for_texel_size(0.125), 3.0);
		let (a, b) = (lod_for_texel_size(0.6), lod_for_texel_size(0.59));
		assert!(a > 0.0 && b > a && b - a < 0.05);
		// Far beyond the old 4-level cap.
		assert!(lod_for_texel_size(MIN_ZOOM_FACTOR) > 13.0);
	}

	#[test]
	fn lod_ignores_unfitted_or_broken_scales() {
		assert_eq!(lod_for_texel_size(0.0), 0.0);
		assert_eq!(lod_for_texel_size(-1.0), 0.0);
		assert_eq!(lod_for_texel_size(f32::NAN), 0.0);
		assert_eq!(lod_for_texel_size(f32::INFINITY), 0.0);
	}
}

#[cfg(test)]
mod svg_tests {
	use super::*;

	#[test]
	fn svg_gets_sharper_only_when_magnified_enough() {
		assert_eq!(sharper_svg_size((1000, 500), 1.0), None);
		assert_eq!(sharper_svg_size((1000, 500), SVG_SHARPEN_TEXEL_SIZE), None);
		assert_eq!(sharper_svg_size((1000, 500), 2.0), Some((2000, 1000)));
		assert_eq!(sharper_svg_size((1000, 500), 4.0), Some((4000, 2000)));
	}

	/// At the size caps there is nothing sharper to ask for, however far it is zoomed: asking
	/// again would never end.
	#[test]
	fn svg_stops_getting_sharper_at_the_caps() {
		assert_eq!(sharper_svg_size((16384, 1000), 8.0), None);
		assert_eq!(sharper_svg_size((8192, 8192), 8.0), None);
		assert!(sharper_svg_size((4096, 4096), 8.0).is_some());
	}
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
	use super::*;

	#[test]
	fn macos_side_buttons_cover_native_and_driver_events() {
		assert_eq!(macos_side_button_direction(MouseButton::Back), Some(-1));
		assert_eq!(macos_side_button_direction(MouseButton::Forward), Some(1));
		assert_eq!(macos_side_button_direction(MouseButton::Other(3)), Some(-1));
		assert_eq!(macos_side_button_direction(MouseButton::Other(4)), Some(1));
	}

	#[test]
	fn swipe_steps_once_per_threshold_and_keeps_direction() {
		let mut accumulated = 0.0;
		accumulated += 0.9;
		assert_eq!(take_swipe_step(&mut accumulated), None);
		accumulated += 1.2;
		assert_eq!(take_swipe_step(&mut accumulated), Some(-1));
		assert_eq!(accumulated, 0.0);
		accumulated -= 2.5;
		assert_eq!(take_swipe_step(&mut accumulated), Some(1));
		assert_eq!(take_swipe_step(&mut accumulated), None);
	}
}
