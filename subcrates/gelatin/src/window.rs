use cgmath::{Matrix4, Vector3};
use glium::{
	Blend, BlendingFunction, Display, Frame, IndexBuffer, Program, Rect, Surface, VertexBuffer,
	glutin::{
		self,
		config::{Api, ConfigSurfaceTypes, GlConfig},
		context::{ContextApi, GlProfile, NotCurrentGlContext, Version},
		display::{GetGlDisplay, GlDisplay},
		surface::{GlSurface, WindowSurface},
	},
	uniform,
};
use log::{debug, error, warn};
use raw_window_handle::HasWindowHandle;
use winit::{
	dpi::{PhysicalPosition, PhysicalSize},
	event::WindowEvent,
	event_loop::ActiveEventLoop,
	keyboard::ModifiersState,
	window::{CursorIcon, Fullscreen, Icon, WindowAttributes, WindowId},
};

#[cfg(target_os = "macos")]
use winit::platform::macos::WindowAttributesExtMacOS;
#[cfg(free_unix)]
use winit::platform::{
	wayland::{ActiveEventLoopExtWayland, WindowAttributesExtWayland},
	x11::{ActiveEventLoopExtX11, WindowAttributesExtX11},
};

use std::{
	cell::{Cell, RefCell, RefMut},
	cmp::Eq,
	fmt::Debug,
	hash::{Hash, Hasher},
	num::NonZeroU32,
	ops::{Deref, DerefMut},
	rc::Rc,
};

use cgmath::ortho;
use derive_builder::Builder;

use crate::shaders;
use crate::{
	DrawContext, Event, EventKind, NextUpdate, Vertex, Widget,
	misc::{FromPhysical, LogicalRect, LogicalVector},
};
use crate::{
	application::Application,
	shaders::{ShaderDescriptor, shader_from_source},
};

const EVENT_UPDATE_DELTA: std::time::Duration = std::time::Duration::from_millis(2);

/// Returns true if and only if of the give window postion is within the boundaries of the display.
fn is_in_bounds(
	display_pos: PhysicalPosition<i32>,
	display_size: PhysicalSize<u32>,
	window_pos: PhysicalPosition<i32>,
) -> bool {
	let is_within_bounds_on_one_axis = |display_pos: i32, display_size: u32, window_pos: i32| {
		// We use this margin so that if the window is too far to the right, and would barely be visible,
		// then we consider it not being on the screen so that the position is reset to 0, 0
		const MARGIN_SIZE: i32 = 40;
		window_pos >= display_pos && window_pos < (display_pos + display_size as i32 - MARGIN_SIZE)
	};
	is_within_bounds_on_one_axis(display_pos.x, display_size.width, window_pos.x)
		&& is_within_bounds_on_one_axis(display_pos.y, display_size.height, window_pos.y)
}

/// Stores whether the window contets need to be re-rendered.
///
/// Widgets must call `invalidate` whenever they go through a
/// a change that requires the widget to be re-drawn.
///
/// This object holds a reference counted bool.
#[derive(Debug, Clone, Default)]
pub struct RenderValidity {
	validity: Rc<Cell<bool>>,
}
impl RenderValidity {
	pub fn invalidate(&self) {
		self.validity.set(false);
	}

	pub fn get(&self) -> bool {
		self.validity.get()
	}

	/// Private accessability because this is only allowed for the window.
	fn make_valid(&self) {
		self.validity.set(true);
	}
}

pub struct WindowDisplayRefMut<'a> {
	window_ref: RefMut<'a, WindowData>,
}
impl<'a> Deref for WindowDisplayRefMut<'a> {
	type Target = Display<WindowSurface>;
	fn deref(&self) -> &Self::Target {
		&self.window_ref.display
	}
}
impl<'a> DerefMut for WindowDisplayRefMut<'a> {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.window_ref.display
	}
}

pub struct WinitWindowRefMut<'a> {
	window_ref: RefMut<'a, WindowData>,
}
impl<'a> Deref for WinitWindowRefMut<'a> {
	type Target = winit::window::Window;
	fn deref(&self) -> &Self::Target {
		&self.window_ref.window
	}
}
impl<'a> DerefMut for WinitWindowRefMut<'a> {
	fn deref_mut(&mut self) -> &mut Self::Target {
		&mut self.window_ref.window
	}
}

#[derive(Builder, Clone)]
#[builder(setter(into))]
pub struct WindowDescriptor {
	#[builder(default)]
	pub icon: Option<Icon>,

	#[builder(default = "PhysicalSize::<u32>::new(800, 600)")]
	pub size: PhysicalSize<u32>,

	/// If true, then `position` and `size` are ignored
	#[builder(default)]
	pub maximized: bool,

	#[builder(default)]
	pub position: Option<PhysicalPosition<i32>>,

	/// Wayland `app_id` / X11 `WM_CLASS` general name. Ignored on other platforms.
	#[builder(default)]
	pub app_id: Option<String>,

	/// Allow the content view to extend into the macOS title bar.
	///
	/// This is ignored on platforms other than macOS. Pair it with
	/// [`titlebar_transparent`](Self::titlebar_transparent) and
	/// [`title_hidden`](Self::title_hidden) to keep the native traffic lights
	/// while drawing content behind the title bar.
	#[builder(default)]
	pub fullsize_content_view: bool,

	/// Make the macOS title bar transparent.
	///
	/// This is ignored on platforms other than macOS. The native title bar
	/// buttons remain available.
	#[builder(default)]
	pub titlebar_transparent: bool,

	/// Hide the macOS title text without hiding the native title bar buttons.
	///
	/// This is ignored on platforms other than macOS.
	#[builder(default)]
	pub title_hidden: bool,
}

pub type EventHandler = dyn FnMut(&Window, &WindowEvent);

struct WindowData {
	display: glium::Display<WindowSurface>,
	window: winit::window::Window,

	size_before_fullscreen: PhysicalSize<u32>,
	fullscreen: bool,
	last_mouse_move_update_time: std::time::Instant,
	unprocessed_move_event: Option<Event>,
	last_event_invalidated: bool,

	new_title: Option<String>,
	/// The last title passed to `set_title`; used to skip redundant updates.
	title: String,
	is_wayland: bool,

	render_validity: RenderValidity,
	cursor_pos: LogicalVector,
	modifiers: ModifiersState,
	root_widget: Rc<dyn Widget>,
	bg_color: [f32; 4],

	global_event_handlers: Vec<Box<EventHandler>>,

	// Draw data
	unit_quad_vertices: VertexBuffer<Vertex>,
	unit_quad_indices: IndexBuffer<u16>,
	textured_program: Program,
	colored_shadowed_program: Program,
	colored_program: Program,
}

pub struct Window {
	data: RefCell<WindowData>,
}
impl Hash for Window {
	fn hash<H: Hasher>(&self, state: &mut H) {
		self.data.as_ptr().hash(state);
	}
}
impl PartialEq for Window {
	fn eq(&self, other: &Window) -> bool {
		self.data.as_ptr() == other.data.as_ptr()
	}
}
impl Eq for Window {}

impl Window {
	/// Call ActiveEventLoop::create_window instead of this directly.
	pub fn new(
		application: &mut Application,
		mut desc: WindowDescriptor,
		event_loop: &winit::event_loop::ActiveEventLoop,
	) -> Result<Rc<Self>, Box<dyn std::error::Error>> {
		const MINIMUM_WINDOW_SIZE: u32 = 200;
		if desc.size.width < MINIMUM_WINDOW_SIZE {
			warn!(
				"Window width was specified to be zero. Defaulting to {MINIMUM_WINDOW_SIZE} instead"
			);
			desc.size.width = MINIMUM_WINDOW_SIZE;
		}
		if desc.size.height < MINIMUM_WINDOW_SIZE {
			warn!(
				"Window height was specified to be zero. Defaulting to {MINIMUM_WINDOW_SIZE} instead"
			);
			desc.size.height = MINIMUM_WINDOW_SIZE;
		}

		let mut window_attributes = winit::window::WindowAttributes::default();
		window_attributes.inner_size = Some(desc.size.into());
		window_attributes.title = "Loading".into();
		window_attributes.fullscreen = None;
		window_attributes.window_icon = desc.icon;
		window_attributes.maximized = desc.maximized;
		window_attributes.visible = false;

		if !desc.maximized {
			window_attributes.inner_size = Some(desc.size.into());
			if let Some(window_pos) = desc.position {
				// Check if the window would be placed outside of the screen
				// (This can happen when using two displays, then disconnecting
				// one of the displays and starting up emulsion)
				let in_bounds = event_loop.available_monitors().any(|monitor| {
					debug!("Monitor pos: {:?}", monitor.position());
					debug!("Monitor size: {:?}", monitor.size());
					is_in_bounds(monitor.position(), monitor.size(), window_pos)
				});
				if in_bounds {
					window_attributes.position = Some(window_pos.into());
				}
			}
		}
		#[cfg(target_os = "macos")]
		{
			window_attributes = window_attributes
				.with_fullsize_content_view(desc.fullsize_content_view)
				.with_titlebar_transparent(desc.titlebar_transparent)
				.with_title_hidden(desc.title_hidden);
		}

		#[cfg(free_unix)]
		let is_wayland = event_loop.is_wayland();
		#[cfg(not(free_unix))]
		let is_wayland = false;

		#[cfg(free_unix)]
		{
			if let Some(app_id) = &desc.app_id {
				let instance = app_id.to_lowercase();
				window_attributes = if is_wayland {
					WindowAttributesExtWayland::with_name(window_attributes, app_id, instance)
				} else if event_loop.is_x11() {
					WindowAttributesExtX11::with_name(window_attributes, app_id, instance)
				} else {
					window_attributes
				};
			}
		}

		let (window, display) = Self::build_winit_window(window_attributes, event_loop)?;

		window.set_cursor(CursorIcon::Default);

		// All the draw stuff
		use glium::index::PrimitiveType;
		let vertex_buffer = VertexBuffer::new(
			&display,
			&[
				Vertex { position: [0.0, 0.0], tex_coords: [0.0, 0.0] },
				Vertex { position: [0.0, 1.0], tex_coords: [0.0, 1.0] },
				Vertex { position: [1.0, 1.0], tex_coords: [1.0, 1.0] },
				Vertex { position: [1.0, 0.0], tex_coords: [1.0, 0.0] },
			],
		)?;

		// building the index buffer
		let index_buffer =
			IndexBuffer::new(&display, PrimitiveType::TriangleStrip, &[1_u16, 2, 0, 3])?;

		// compiling shaders and linking them together
		let textured_program = shader_from_source(
			&display,
			ShaderDescriptor {
				vertex_shader: shaders::VERTEX_140,
				fragment_shader: shaders::TEXTURE_SHADOW_F_140,
				outputs_srgb: false,
			},
		)?;

		let colored_shadowed_program = shader_from_source(
			&display,
			ShaderDescriptor {
				vertex_shader: shaders::VERTEX_140,
				fragment_shader: shaders::COLOR_SHADOW_F_140,
				outputs_srgb: false,
			},
		)?;

		let colored_program = shader_from_source(
			&display,
			ShaderDescriptor {
				vertex_shader: shaders::VERTEX_140,
				fragment_shader: shaders::COLOR_F_140,
				outputs_srgb: false,
			},
		)?;

		let resulting_window = Rc::new(Window {
			data: RefCell::new(WindowData {
				display,
				window,
				size_before_fullscreen: desc.size,
				fullscreen: false,
				last_mouse_move_update_time: std::time::Instant::now(),
				unprocessed_move_event: None,
				last_event_invalidated: true,
				new_title: None,
				title: "Loading".into(),
				is_wayland,
				cursor_pos: Default::default(),
				modifiers: ModifiersState::empty(),
				render_validity: RenderValidity { validity: Rc::new(Cell::new(false)) },
				root_widget: Rc::new(crate::line_layout_container::VerticalLayoutContainer::new()),
				bg_color: [0.85, 0.85, 0.85, 1.0],

				global_event_handlers: Vec::new(),

				unit_quad_vertices: vertex_buffer,
				unit_quad_indices: index_buffer,
				textured_program,
				colored_shadowed_program,
				colored_program,
			}),
		});

		resulting_window.data.borrow_mut().window.set_visible(true);

		application.register_window(resulting_window.clone());
		Ok(resulting_window)
	}

	/// This is mostly copy-pasted from `glutin::SimpleWindowBuilder::build`
	/// but I use some custom configuration settings here
	fn build_winit_window(
		attributes: WindowAttributes,
		event_loop: &ActiveEventLoop,
	) -> Result<(winit::window::Window, Display<WindowSurface>), Box<dyn std::error::Error>> {
		// First we start by opening a new Window
		let display_builder =
			glutin_winit::DisplayBuilder::new().with_window_attributes(Some(attributes));

		let config_template_builder = glutin::config::ConfigTemplateBuilder::new()
			.prefer_hardware_accelerated(Some(true))
			.with_surface_type(ConfigSurfaceTypes::WINDOW)
			.with_api(Api::OPENGL);
		let (window, gl_config) = display_builder
			.build(event_loop, config_template_builder, |mut configs| {
				// Pick the first srgb capable config
				// ponytail: the picker cannot return an error, so an empty config list panics here;
				// glutin's EGL/GLX/WGL backends fail earlier with `NotFound` instead of yielding none.
				let mut target = configs.next().expect("glutin returned no OpenGL configs");
				for cfg in configs {
					if target.srgb_capable() {
						break;
					}
					target = cfg;
				}
				target
			})
			.map_err(|e| {
				format!("Failed to find a suitable OpenGL config (OpenGL 3.3 is required): {e}")
			})?;
		let window = window.ok_or("Failed to create the window")?;

		let raw_window_handle = window
			.window_handle()
			.map_err(|e| format!("Failed to get the native window handle: {e}"))?
			.as_raw();

		// Now we get the window size to use as the initial size of the Surface
		let (width, height): (u32, u32) = window.inner_size().into();
		let attrs =
			glutin::surface::SurfaceAttributesBuilder::<glutin::surface::WindowSurface>::new()
				.with_srgb(Some(true))
				.build(
					raw_window_handle,
					NonZeroU32::new(width.max(1)).unwrap(),
					NonZeroU32::new(height.max(1)).unwrap(),
				);

		// Finally we can create a Surface, use it to make a PossiblyCurrentContext and create the glium Display
		let surface = unsafe { gl_config.display().create_window_surface(&gl_config, &attrs) }
			.map_err(|e| format!("Failed to create the OpenGL window surface: {e}"))?;

		let context_attributes = glutin::context::ContextAttributesBuilder::new()
			.with_profile(GlProfile::Core) // requires OpenGL 3.3
			.with_context_api(ContextApi::OpenGl(Some(Version::new(3, 3))))
			.with_release_behavior(glutin::context::ReleaseBehavior::None)
			.build(Some(raw_window_handle));
		let current_context =
			unsafe { gl_config.display().create_context(&gl_config, &context_attributes) }
				.map_err(|e| {
					format!(
						"Failed to create an OpenGL 3.3 core context (OpenGL 3.3 is required): {e}"
					)
				})?
				.make_current(&surface)
				.map_err(|e| format!("Failed to make the OpenGL context current: {e}"))?;

		// Set up vsync
		let swap_interval = glutin::surface::SwapInterval::Wait(NonZeroU32::new(1).unwrap());
		if let Err(error) = surface.set_swap_interval(&current_context, swap_interval) {
			error!("Failed to set vsync, error returned by set_swap_interval: {}", error);
		}

		let display = Display::from_context_surface(current_context, surface)
			.map_err(|e| format!("Failed to initialize the OpenGL display: {e}"))?;

		Ok((window, display))
	}

	pub fn add_global_event_handler<F: FnMut(&Window, &WindowEvent) + 'static>(&self, fun: F) {
		let mut borrowed = self.data.borrow_mut();
		borrowed.global_event_handlers.push(Box::new(fun));
	}

	pub fn set_root<T: Widget>(&self, widget: Rc<T>) {
		let mut borrowed = self.data.borrow_mut();
		widget.set_valid_ref(borrowed.render_validity.clone());
		borrowed.root_widget = widget;
		borrowed.render_validity.invalidate();
	}

	pub fn set_bg_color(&self, color: [f32; 4]) {
		let mut borrowed = self.data.borrow_mut();
		borrowed.bg_color = color;
	}

	/// This is called when a NewEvents event is received in the application
	pub fn handle_loop_wake_up(&self) -> NextUpdate {
		let root_widget = self.data.borrow().root_widget.clone();
		root_widget.before_draw(self)
	}

	pub fn process_event<UserEvent>(&self, native_event: WindowEvent) {
		use winit::event::MouseScrollDelta;

		let event;
		// Enable with RUST_LOG=gelatin=trace to see exactly what the OS delivers,
		// which is the only reliable way to tell a missing binding from a mouse
		// driver that swallows the button.
		log::trace!("Window event: {native_event:?}");
		{
			let mut event_handlers = Vec::new();
			std::mem::swap(&mut event_handlers, &mut self.data.borrow_mut().global_event_handlers);
			for handler in event_handlers.iter_mut() {
				handler(self, &native_event);
			}
			let mut borrowed = self.data.borrow_mut();
			// We don't expect any new event handlers to have been added to the window by the event handlers
			assert!(borrowed.global_event_handlers.is_empty());
			std::mem::swap(&mut event_handlers, &mut borrowed.global_event_handlers);
			match native_event {
				WindowEvent::Resized(size) => {
					event = None;
					if size.width < 1 || size.height < 1 {
						return;
					}

					if borrowed.is_wayland {
						// We need to set the framebuffer size explicitly on Wayland (god knows why)
						borrowed.display.resize((size.width, size.height));
					}
					borrowed.window.request_redraw();
				}
				WindowEvent::ScaleFactorChanged { .. } => {
					event = None;
					borrowed.render_validity.invalidate();
					borrowed.window.request_redraw();
				}
				WindowEvent::CloseRequested => {
					event = Some(Event {
						cursor_pos: borrowed.cursor_pos,
						modifiers: borrowed.modifiers,
						kind: EventKind::CloseRequested,
					});
				}
				WindowEvent::KeyboardInput { event: key_event, .. } => {
					event = Some(Event {
						cursor_pos: borrowed.cursor_pos,
						modifiers: borrowed.modifiers,
						kind: EventKind::KeyInput { input: key_event },
					});
				}
				WindowEvent::CursorMoved { position, .. } => {
					let logical_pos;
					{
						let scaling = borrowed.window.scale_factor() as f32;

						logical_pos = LogicalVector::from_physical(position, scaling);
						//logical_pos.vec.y = logical_dimensions.vec.y - logical_pos.vec.y;
					}
					borrowed.cursor_pos = logical_pos;
					let move_event = Event {
						cursor_pos: borrowed.cursor_pos,
						modifiers: borrowed.modifiers,
						kind: EventKind::MouseMove,
					};
					let last_update_elapsed = borrowed.last_mouse_move_update_time.elapsed();
					if borrowed.last_event_invalidated || last_update_elapsed > EVENT_UPDATE_DELTA {
						borrowed.last_mouse_move_update_time = std::time::Instant::now();
						event = Some(move_event);
					} else {
						event = None;
						borrowed.unprocessed_move_event = Some(move_event);
					}
				}
				WindowEvent::MouseWheel { delta: native_delta, .. } => {
					let delta = match native_delta {
						MouseScrollDelta::LineDelta(x, y) => LogicalVector::new(x, y),
						MouseScrollDelta::PixelDelta(native_pos) => LogicalVector::new(
							native_pos.x as f32 / 13.0,
							native_pos.y as f32 / 8.0,
						),
					};
					event = Some(Event {
						cursor_pos: borrowed.cursor_pos,
						modifiers: borrowed.modifiers,
						kind: EventKind::MouseScroll { delta },
					});
				}
				WindowEvent::MouseInput { state, button, .. } => {
					event = Some(Event {
						cursor_pos: borrowed.cursor_pos,
						modifiers: borrowed.modifiers,
						kind: EventKind::MouseButton { state, button },
					});
				}
				WindowEvent::DroppedFile(path) => {
					event = Some(Event {
						cursor_pos: borrowed.cursor_pos,
						modifiers: borrowed.modifiers,
						kind: EventKind::DroppedFile(path),
					});
				}
				WindowEvent::HoveredFile(path) => {
					event = Some(Event {
						cursor_pos: borrowed.cursor_pos,
						modifiers: borrowed.modifiers,
						kind: EventKind::HoveredFile(path),
					});
				}
				WindowEvent::HoveredFileCancelled => {
					event = Some(Event {
						cursor_pos: borrowed.cursor_pos,
						modifiers: borrowed.modifiers,
						kind: EventKind::HoveredFileCancelled,
					});
				}
				WindowEvent::Focused(focused) => {
					event = Some(Event {
						cursor_pos: borrowed.cursor_pos,
						modifiers: borrowed.modifiers,
						kind: EventKind::Focused(focused),
					});
				}
				WindowEvent::ModifiersChanged(modifiers) => {
					borrowed.modifiers = modifiers.state();
					event = None;
				}
				_ => event = None,
			}
		}

		if let Some(event) = event {
			let cloned = self.data.borrow().root_widget.clone();
			cloned.handle_event(&event);
			let mut borrowed = self.data.borrow_mut();
			if !borrowed.render_validity.get() {
				borrowed.last_event_invalidated = true;
			}
		}
	}

	pub fn set_title(&self, title: String) {
		// Deferring to set the title later, because
		// the program sometimes crashes on wayland if this is
		// done in the `MainEventsCleared` event.
		let mut borrowed = self.data.borrow_mut();
		if borrowed.title == title {
			return;
		}
		borrowed.title.clone_from(&title);
		borrowed.new_title = Some(title);
		borrowed.render_validity.invalidate();
	}

	pub fn display_mut(&self) -> WindowDisplayRefMut<'_> {
		WindowDisplayRefMut { window_ref: self.data.borrow_mut() }
	}

	pub fn window_mut(&self) -> WinitWindowRefMut<'_> {
		WinitWindowRefMut { window_ref: self.data.borrow_mut() }
	}

	pub fn get_id(&self) -> WindowId {
		self.data.borrow().window.id()
	}

	pub fn request_redraw(&self) {
		self.data.borrow_mut().window.request_redraw();
	}

	/// Start a native window drag.
	///
	/// Applications can use this from a pointer handler for a custom top drag
	/// strip. The operating system decides whether the current pointer event
	/// can start a drag.
	pub fn drag_window(&self) -> Result<(), winit::error::ExternalError> {
		self.data.borrow().window.drag_window()
	}

	pub fn set_visible(&self, visible: bool) {
		self.data.borrow().window.set_visible(visible);
	}

	pub fn focus_window(&self) {
		self.data.borrow().window.focus_window();
	}

	pub fn theme(&self) -> Option<winit::window::Theme> {
		self.data.borrow().window.theme()
	}

	pub fn main_events_cleared(&self) {
		let root_widget = self.data.borrow().root_widget.clone();
		let event = self.data.borrow_mut().unprocessed_move_event.take();
		if let Some(event) = event {
			root_widget.handle_event(&event);
		}
	}

	pub fn redraw_needed(&self) -> bool {
		!self.data.borrow().render_validity.get()
	}

	/// WARNING The window may not be changed during the drawing phase.
	/// This means that trying to borrow the window *mutably* in a widget's
	/// draw function will fail.
	pub fn redraw(&self) -> crate::NextUpdate {
		// Using a scope to only borrow the data mutably for the very beggining.
		{
			let mut borrowed = self.data.borrow_mut();
			if let Some(new_title) = borrowed.new_title.take() {
				borrowed.window.set_title(&new_title);
			}
			borrowed.last_event_invalidated = false;
		}
		// this way self.data is not borrowed while before draw is running.
		let dpi_scaling = self.data.borrow().window.scale_factor();
		let mut target = self.data.borrow().display.draw();

		// Can't change the window during drawing phase. Deal with it.
		let borrowed = self.data.borrow();
		let dimensions = target.get_dimensions();
		let phys_dimensions = PhysicalSize::new(dimensions.0 as f32, dimensions.1 as f32);
		let phys_width = phys_dimensions.width;
		let phys_height = phys_dimensions.height;
		let logical_dimensions = LogicalVector::from_physical(phys_dimensions, dpi_scaling as f32);

		// Invoke the layout functions
		let available_widget_space =
			LogicalRect { pos: LogicalVector::new(0.0, 0.0), size: logical_dimensions };
		borrowed.root_widget.layout(available_widget_space);

		let left = 0f32;
		let right = logical_dimensions.vec.x;
		let bottom = logical_dimensions.vec.y;
		let top = 0f32;
		let projection_transform = ortho(left, right, bottom, top, -1f32, 1f32);

		let viewport = Rect {
			left: 0_u32,
			width: phys_width as u32,
			bottom: 0_u32,
			height: phys_height as u32,
		};

		let draw_context = DrawContext {
			display: &borrowed.display,
			dpi_scale_factor: dpi_scaling as f32,
			unit_quad_vertices: &borrowed.unit_quad_vertices,
			unit_quad_indices: &borrowed.unit_quad_indices,
			textured_program: &borrowed.textured_program,
			colored_shadowed_program: &borrowed.colored_shadowed_program,
			colored_program: &borrowed.colored_program,
			viewport: &viewport,
			projection_transform: &projection_transform,
		};

		// Drawing a full-screen quad to emulate colored clearing instead of
		// `Surface::clear_color`. This is a workaround for
		// https://github.com/glium/glium/issues/1842. The window is always opaque, so the
		// quad overwrites every pixel and a preceding clear would be redundant.
		let [r, g, b, _] = borrowed.bg_color;
		draw_context.clear_color(&mut target, [r, g, b, 1.0], None);

		// Using the cloned root instead of self.root_widget doesn't make much difference
		// because self is being borrowed by through the draw_context anyways but it's fine.
		let next_update = match borrowed.root_widget.draw(&mut target, &draw_context) {
			Ok(next_update) => next_update,
			Err(e) => {
				error!("Failed to draw the widgets: {e}");
				NextUpdate::Latest
			}
		};

		// After all widgets are drawn, let's set the alpha values of all the pixels to 1.
		// This is required on Wayland because the Wayland compositor very kindly takes
		// the alpha values into account and blends the framebuffer set by applications
		// with the rest of the desktop. Other platforms ignore the framebuffer alpha.
		if borrowed.is_wayland {
			self.set_alpha_to_1(&mut target, &draw_context);
		}

		if let Err(e) = target.finish() {
			error!("Failed to swap the buffers: {e}");
		}
		borrowed.render_validity.make_valid();
		next_update
	}

	pub fn fullscreen(&self) -> bool {
		self.data.borrow().fullscreen
	}

	pub fn set_fullscreen(&self, fullscreen: bool) {
		let mut borrowed = self.data.borrow_mut();
		borrowed.fullscreen = fullscreen;
		let monitor = if fullscreen {
			let curr_mon;
			borrowed.size_before_fullscreen = {
				curr_mon = borrowed.window.current_monitor();
				borrowed.window.inner_size()
			};
			Some(Fullscreen::Borderless(curr_mon))
		} else {
			None
		};
		borrowed.window.set_fullscreen(monitor);
	}

	/// Sets the alpha values by drawing a quad covering the entire framebuffer
	/// with a blending mode set to max and a shader that draws (0,0,0,1) values
	fn set_alpha_to_1(&self, target: &mut Frame, context: &DrawContext) {
		let transform = Matrix4::from_scale(2.0);
		let transform = Matrix4::from_translation(Vector3::new(-1.0, -1.0, 0.0)) * transform;
		let image_draw_params = glium::DrawParameters {
			blend: Blend {
				color: BlendingFunction::Max,
				alpha: BlendingFunction::Max,
				..Default::default()
			},
			..Default::default()
		};
		let uniforms = uniform! {
			matrix: Into::<[[f32; 4]; 4]>::into(transform),
			color: [0.0f32, 0.0, 0.0, 1.0],
		};
		if let Err(e) = target.draw(
			context.unit_quad_vertices,
			context.unit_quad_indices,
			context.colored_program,
			&uniforms,
			&image_draw_params,
		) {
			error!("Failed to set the framebuffer alpha: {e}");
		}
	}
}
