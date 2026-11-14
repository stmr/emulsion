#![cfg_attr(all(not(feature = "benchmark"), not(debug_assertions)), windows_subsystem = "windows")]

use std::cell::{Cell, RefCell};
use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use directories_next::ProjectDirs;
use gelatin::application::ApplicationHandler;
use gelatin::event_loop::{self, ActiveEventLoop};

use log::{debug, trace};

use gelatin::winit::{
	dpi::{PhysicalPosition, PhysicalSize},
	event::WindowEvent,
	window::Icon,
};
use gelatin::{
	NextUpdate, Widget,
	application::*,
	button::*,
	image,
	label::*,
	line_layout_container::*,
	misc::*,
	picture::*,
	window::{Window, WindowDescriptorBuilder},
};

use crate::configuration::Theme;
use crate::configuration::{Cache, ConfigWindowSection, Configuration};
use crate::version::Version;
use crate::widgets::{
	bottom_bar::BottomBar,
	copy_notification::CopyNotifications,
	help_screen::*,
	picture_widget::*,
	thumbnail_stripe::{DEFAULT_THUMBNAIL_SIZE, StripeMode, ThumbnailStripe},
};
mod clipboard_handler;
mod cmd_line;
mod configuration;
mod handle_panic;
mod image_cache;
mod input_handling;
#[cfg(target_os = "macos")]
mod macos;
mod parallel_action;
mod playback_manager;
mod shaders;
mod system_memory;
mod utils;
mod version;
mod widgets;

// The program name will be 'emulsion' (i.e. starting with a lower-case letter) on Linux
pub static PROJECT_DIRS: std::sync::LazyLock<Option<ProjectDirs>> =
	std::sync::LazyLock::new(|| ProjectDirs::from("", "", "Emulsion"));

static NEW_VERSION: &[u8] = include_bytes!("../resource/new-version-available.png");
static NEW_VERSION_LIGHT: &[u8] = include_bytes!("../resource/new-version-available-light.png");
static VISIT_SITE: &[u8] = include_bytes!("../resource/visit-site.png");
static USAGE: &[u8] = include_bytes!("../resource/usage.png");
static LEFT_TO_PAN: &[u8] = include_bytes!("../resource/use-left-to-pan.png");
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHT_ZONE_WIDTH: f64 = 120.0;
#[cfg(target_os = "macos")]
const TRAFFIC_LIGHT_ZONE_HEIGHT: f64 = 52.0;
#[cfg(target_os = "macos")]
const CHROME_HIDE_DELAY: Duration = Duration::from_millis(800);

#[derive(Debug)]
pub enum EmulsionEvent {
	OpenPaths(Vec<PathBuf>),
	Reopen,
	Command(String),
}

// ========================================================
// Not-so glorious main function
// ========================================================
fn main() {
	std::panic::set_hook(Box::new(handle_panic::handle_panic));
	env_logger::init();
	trace!("Starting up. Panic hook set, logger initialized.");

	// Load configuration and cache files
	let (config_path, cache_path) = get_config_and_cache_paths();

	let args = cmd_line::parse_args(&config_path, &cache_path);

	let first_launch = !cache_path.exists();
	let cache = load_or_default(&cache_path, Cache::load(&cache_path));
	let config = load_or_default(&config_path, Configuration::load(&config_path));

	debug!("Read cache: {cache:#?}");
	debug!("Read config: {config:#?}");

	let cache = Arc::new(Mutex::new(cache));
	let config = Rc::new(RefCell::new(config));

	if args.displayed_folders.is_some() {
		config.borrow_mut().title.get_or_insert_with(Default::default).displayed_folders =
			args.displayed_folders;
	}

	let mut application = Application::new();

	let app_handler = AppHandler {
		cache_path,
		args,
		first_launch,
		cache: cache.clone(),
		config: config.clone(),
		update_presented: false,
		update_checker_join_handle: None,

		update_available: Arc::new(AtomicBool::new(false)),
		update_check_done: Arc::new(AtomicBool::new(false)),
		ui_elements: None,
		pending_open: Vec::new(),
		#[cfg(target_os = "macos")]
		traffic_lights_visible: false,
		#[cfg(target_os = "macos")]
		traffic_lights_hide_at: None,
	};

	let event_loop = event_loop::EventLoop::<EmulsionEvent>::new();
	#[cfg(target_os = "macos")]
	let _mac_integration = macos::install(event_loop.create_proxy());

	application.start_event_loop(app_handler, event_loop);
}

/// A missing file is normal (first launch, no user config); an unreadable or invalid one is
/// reported so the user can find the typo.
fn load_or_default<T: Default>(path: &Path, loaded: Result<T, String>) -> T {
	loaded.unwrap_or_else(|error| {
		if path.exists() {
			log::error!("{error}; using defaults");
		} else {
			debug!("{error}; using defaults");
		}
		T::default()
	})
}

struct UiElements {
	theme: Rc<Cell<Theme>>,
	set_theme: Rc<dyn Fn()>,
	window: Rc<Window>,
	picture_widget: Rc<PictureWidget>,

	update_notification: Rc<HorizontalLayoutContainer>,
	help_screen: Rc<HelpScreen>,
}

struct AppHandler {
	cache_path: PathBuf,

	args: cmd_line::Args,
	first_launch: bool,
	cache: Arc<Mutex<Cache>>,
	config: Rc<RefCell<Configuration>>,

	update_available: Arc<AtomicBool>,
	update_check_done: Arc<AtomicBool>,
	/// True when an update was checed and in case an update is available, it was presented to the user
	update_presented: bool,
	update_checker_join_handle: Option<JoinHandle<()>>,

	ui_elements: Option<UiElements>,
	/// Paths from Finder that arrived before the window existed.
	pending_open: Vec<PathBuf>,
	#[cfg(target_os = "macos")]
	traffic_lights_visible: bool,
	#[cfg(target_os = "macos")]
	traffic_lights_hide_at: Option<Instant>,
}

impl AppHandler {
	fn create_window(
		&mut self,
		event_loop: &mut ActiveEventLoop,
		args: cmd_line::Args,
		first_launch: bool,
		cache: Arc<Mutex<Cache>>,
		config: Rc<RefCell<Configuration>>,
	) -> Option<JoinHandle<()>> {
		let window: Rc<Window> = {
			let window_cache = &mut cache.lock().unwrap().window;
			let window_cfg = &config.borrow().window;
			let window_defaults = configuration::CacheWindowSection::default();

			if let Some(ConfigWindowSection {
				use_last_window_area: Some(false),
				win_x,
				win_y,
				win_w,
				win_h,
				..
			}) = window_cfg
			{
				window_cache.win_x = if let Some(x) = win_x { *x } else { window_defaults.win_x };
				window_cache.win_y = if let Some(y) = win_y { *y } else { window_defaults.win_y };
				window_cache.win_w = if let Some(w) = win_w { *w } else { window_defaults.win_w };
				window_cache.win_h = if let Some(h) = win_h { *h } else { window_defaults.win_h };
			}

			if let Some(window_cfg) = window_cfg
				&& let Some(start_maximized) = window_cfg.start_maximized
			{
				window_cache.maximized = start_maximized;
			}

			let pos = PhysicalPosition::new(window_cache.win_x, window_cache.win_y);
			let size = PhysicalSize::new(window_cache.win_w, window_cache.win_h);
			let window_desc = WindowDescriptorBuilder::default()
				.icon(Some(make_icon()))
				.maximized(window_cache.maximized)
				.size(size)
				.position(Some(pos))
				.app_id(Some("Emulsion".into()))
				.fullsize_content_view(cfg!(target_os = "macos"))
				.titlebar_transparent(cfg!(target_os = "macos"))
				.title_hidden(cfg!(target_os = "macos"))
				.build()
				.unwrap_or_else(|e| fatal_startup_error(&format!("Invalid window settings: {e}")));

			let window = event_loop.create_window(window_desc).unwrap_or_else(|e| {
				fatal_startup_error(&format!("Could not create the window: {e}"))
			});

			if let Some(ConfigWindowSection { start_fullscreen: Some(true), .. }) = window_cfg {
				window.set_fullscreen(true);
			}
			window
		};
		#[cfg(target_os = "macos")]
		{
			macos::use_display_p3_color_space(&window);
			macos::set_traffic_lights_visible(&window, false);
		}
		add_window_movement_listener(&window, cache.clone());

		let update_label_image = Rc::new(Picture::from_encoded_bytes(NEW_VERSION));
		let update_label_image_light = Rc::new(Picture::from_encoded_bytes(NEW_VERSION_LIGHT));
		let update_label = make_update_label();

		let update_notification = make_update_notification(update_label.clone());

		let usage_img = Picture::from_encoded_bytes(USAGE);
		let help_screen = Rc::new(HelpScreen::new(usage_img));
		let left_to_pan_img = Picture::from_encoded_bytes(LEFT_TO_PAN);
		let left_to_pan_hint = Rc::new(HelpScreen::new(left_to_pan_img));

		let copy_notifications_widget = Rc::new(Label::new());
		let copy_notifications = CopyNotifications::new(&copy_notifications_widget);

		let bottom_bar = Rc::new(BottomBar::new(&config.borrow()));
		let picture_widget = make_picture_widget(
			&window,
			bottom_bar.clone(),
			left_to_pan_hint.clone(),
			copy_notifications,
			config.clone(),
			cache.clone(),
		);

		// The system hands over Finder/Dock documents through `openFile:`, which may land
		// before the window exists.
		let startup_path = args
			.file_path
			.as_ref()
			.map(PathBuf::from)
			.or_else(|| self.pending_open.drain(..).next());
		if let Some(path) = startup_path {
			picture_widget.jump_to_path(path);
		}

		let picture_area_container = make_picture_area_container();
		picture_area_container.add_child(picture_widget.clone());
		picture_area_container.add_child(copy_notifications_widget);
		picture_area_container.add_child(left_to_pan_hint);
		picture_area_container.add_child(help_screen.clone());
		let thumbnail_mode = {
			let config = config.borrow();
			let configured = config
				.window
				.as_ref()
				.and_then(|window| window.show_thumbnails.as_deref())
				.map(StripeMode::parse);
			configured
				.unwrap_or_else(|| StripeMode::parse(&cache.lock().unwrap().window.thumbnails))
		};
		let thumbnail_size = config
			.borrow()
			.window
			.as_ref()
			.and_then(|window| window.thumbnail_size)
			.unwrap_or(DEFAULT_THUMBNAIL_SIZE)
			.clamp(48.0, 256.0);
		let thumbnail_stripe = Rc::new(ThumbnailStripe::new(thumbnail_size));
		thumbnail_stripe.set_visible(thumbnail_mode == StripeMode::Pinned);
		{
			let picture_widget = picture_widget.clone();
			thumbnail_stripe.set_on_select(move |index| {
				picture_widget.jump_to_index(index as u32);
			});
		}
		picture_widget.set_thumbnail_stripe(thumbnail_stripe.clone(), thumbnail_mode);
		picture_area_container.add_child(update_notification.clone());
		picture_area_container.add_child(thumbnail_stripe.clone());
		picture_area_container.add_child(bottom_bar.widget.clone());

		let root_container = make_root_container();
		root_container.add_child(picture_area_container);

		self.update_available = Arc::new(AtomicBool::new(false));
		self.update_check_done = Arc::new(AtomicBool::new(false));

		let theme = {
			let cached = cache.lock().unwrap();
			let system_theme = || match window.theme() {
				Some(gelatin::winit::window::Theme::Dark) => Theme::Dark,
				Some(gelatin::winit::window::Theme::Light) => Theme::Light,
				None => cached.theme(),
			};
			Rc::new(Cell::new(match &config.borrow().window {
				Some(ConfigWindowSection { theme: Some(theme_cfg), .. }) => *theme_cfg,
				_ if cached.window.follow_system_theme => system_theme(),
				_ => cached.theme(),
			}))
		};

		let set_theme = {
			let update_label = update_label.clone();
			let picture_widget = picture_widget.clone();
			let update_notification = update_notification.clone();
			let window = window.clone();
			let theme = theme.clone();
			let update_available = self.update_available.clone();
			let bottom_bar = bottom_bar.clone();

			Rc::new(move || {
				match theme.get() {
					Theme::Light => {
						picture_widget.set_bright_shade(0.96);
						window.set_bg_color([0.85, 0.85, 0.85, 1.0]);
						update_notification.set_bg_color([0.06, 0.06, 0.06, 1.0]);
						update_label.set_icon(Some(update_label_image_light.clone()));
					}
					Theme::Dark => {
						picture_widget.set_bright_shade(0.11);
						window.set_bg_color([0.03, 0.03, 0.03, 1.0]);
						update_notification.set_bg_color([0.85, 0.85, 0.85, 1.0]);
						update_label.set_icon(Some(update_label_image.clone()));
					}
				}
				bottom_bar.set_theme(theme.get(), update_available.load(Ordering::SeqCst));
			})
		};
		set_theme();
		{
			let cache = cache.clone();
			let set_theme = set_theme.clone();
			let theme = theme.clone();
			bottom_bar.theme_button.set_on_click(move || {
				let new_theme = theme.get().switch_theme();
				theme.set(new_theme);
				{
					let mut cache = cache.lock().unwrap();
					cache.set_theme(new_theme);
					cache.window.follow_system_theme = false;
				}
				set_theme();
			});
		}
		{
			let slider = bottom_bar.slider.clone();
			let picture_widget = picture_widget.clone();
			bottom_bar.slider.set_on_value_change(move || {
				picture_widget.jump_to_index(slider.value());
			});
		}
		{
			let gamma_slider = bottom_bar.gamma_slider.clone();
			let picture_widget = picture_widget.clone();
			bottom_bar.gamma_slider.set_on_value_change(move || {
				picture_widget.set_gamma_from_slider(gamma_slider.value());
			});
		}
		{
			let picture_widget = picture_widget.clone();
			bottom_bar.gamma_slider.set_on_double_click(move || {
				picture_widget.reset_gamma();
			});
		}
		{
			let picture_widget = picture_widget.clone();
			bottom_bar.orig_scale_button.set_on_click(move || {
				picture_widget.set_img_size_to_orig();
			});
		}
		{
			let picture_widget = picture_widget.clone();
			bottom_bar.fit_best_button.set_on_click(move || {
				picture_widget.set_img_size_to_fit(false);
			});
		}
		{
			let picture_widget = picture_widget.clone();
			bottom_bar.fit_stretch_button.set_on_click(move || {
				picture_widget.set_img_size_to_fit(true);
			});
		}
		let help_visible = Cell::new(first_launch && args.file_path.is_none());
		help_screen.set_visible(help_visible.get());
		update_notification
			.set_visible(help_visible.get() && self.update_available.load(Ordering::SeqCst));
		{
			let update_available = self.update_available.clone();
			let help_screen = help_screen.clone();
			let update_notification = update_notification.clone();
			let bottom_bar_clone = bottom_bar.clone();

			bottom_bar.help_button.set_on_click(move || {
				help_visible.set(!help_visible.get());
				help_screen.set_visible(help_visible.get());
				bottom_bar_clone.set_help_visible(help_visible.get());
				update_notification
					.set_visible(help_visible.get() && update_available.load(Ordering::SeqCst));
			});
		}

		window.set_root(root_container);

		let check_updates_enabled =
			config.borrow().updates.as_ref().map(|u| u.check_updates).unwrap_or(true);

		let update_checker_join_handle = {
			let updates = &mut cache.lock().unwrap().updates;
			let cache = cache.clone();
			let update_available = self.update_available.clone();
			let update_check_done = self.update_check_done.clone();

			if check_updates_enabled && updates.update_check_needed() {
				// kick off a thread that will check for an update in the background
				Some(std::thread::spawn(move || {
					let has_update = update::check_for_updates();
					update_available.store(has_update, Ordering::SeqCst);
					update_check_done.store(true, Ordering::SeqCst);
					if !has_update {
						cache.lock().unwrap().updates.set_update_check_time();
					}
				}))
			} else {
				None
			}
		};

		self.ui_elements = Some(UiElements {
			theme,
			set_theme,
			window,
			picture_widget,
			update_notification,
			help_screen,
		});

		update_checker_join_handle
	}
}

impl ApplicationHandler<EmulsionEvent> for AppHandler {
	fn handle_can_create_surface(&mut self, event_loop: &mut ActiveEventLoop) {
		self.update_checker_join_handle = self.create_window(
			event_loop,
			self.args.clone(),
			self.first_launch,
			self.cache.clone(),
			self.config.clone(),
		);
	}

	fn handle_window_event(
		&mut self,
		_event_loop: &ActiveEventLoop,
		_window_id: gelatin::winit::window::WindowId,
		event: &WindowEvent,
	) -> NextUpdate {
		if let Some(ui) = &self.ui_elements {
			#[cfg(target_os = "macos")]
			if let WindowEvent::CursorMoved { position, .. } = event {
				let scale = ui.window.window_mut().scale_factor();
				let near_controls = position.x / scale <= TRAFFIC_LIGHT_ZONE_WIDTH
					&& position.y / scale <= TRAFFIC_LIGHT_ZONE_HEIGHT
					&& !ui.window.fullscreen();
				if near_controls {
					self.traffic_lights_hide_at = None;
					if !self.traffic_lights_visible {
						macos::set_traffic_lights_visible(&ui.window, true);
						self.traffic_lights_visible = true;
					}
				} else if self.traffic_lights_visible && self.traffic_lights_hide_at.is_none() {
					self.traffic_lights_hide_at = Some(Instant::now() + CHROME_HIDE_DELAY);
				}
			}
			if let WindowEvent::ThemeChanged(system_theme) = event {
				let follows_system = self
					.config
					.borrow()
					.window
					.as_ref()
					.is_none_or(|window| window.theme.is_none())
					&& self.cache.lock().unwrap().window.follow_system_theme;
				if follows_system {
					ui.theme.set(match system_theme {
						gelatin::winit::window::Theme::Dark => Theme::Dark,
						gelatin::winit::window::Theme::Light => Theme::Light,
					});
					(ui.set_theme)();
				}
			}
			if matches!(event, WindowEvent::CloseRequested)
				&& cfg!(target_os = "macos")
				&& !self
					.config
					.borrow()
					.window
					.as_ref()
					.and_then(|window| window.quit_on_close)
					.unwrap_or(false)
			{
				ui.window.set_visible(false);
			}
			if !self.update_presented && self.update_check_done.load(Ordering::SeqCst) {
				self.update_presented = true;
				(ui.set_theme)();
				if ui.help_screen.visible() && self.update_available.load(Ordering::SeqCst) {
					ui.update_notification.set_visible(true);
				}
			}
		}
		let next_update = NextUpdate::WaitUntil(Instant::now() + Duration::from_secs(1));
		#[cfg(target_os = "macos")]
		let next_update = self
			.traffic_lights_hide_at
			.map_or(next_update, |hide_at| next_update.aggregate(NextUpdate::WaitUntil(hide_at)));
		next_update
	}
	fn handle_user_event(&mut self, event: EmulsionEvent) {
		if let EmulsionEvent::OpenPaths(paths) = &event
			&& self.ui_elements.is_none()
		{
			self.pending_open.extend(paths.iter().cloned());
			return;
		}
		let Some(ui) = &self.ui_elements else {
			return;
		};
		match event {
			EmulsionEvent::OpenPaths(paths) => {
				if let Some(path) = paths.into_iter().next() {
					ui.picture_widget.jump_to_path(path);
					ui.window.set_visible(true);
					ui.window.focus_window();
				}
			}
			EmulsionEvent::Reopen => {
				ui.window.set_visible(true);
				ui.window.focus_window();
			}
			EmulsionEvent::Command(command) => match command.as_str() {
				"open" =>
				{
					#[cfg(target_os = "macos")]
					if let Some(path) = macos::choose_image() {
						ui.picture_widget.jump_to_path(path);
						ui.window.set_visible(true);
						ui.window.focus_window();
					}
				}
				"show_help" => ui.help_screen.set_visible(true),
				"visit_site" => {
					if let Err(error) = open::that("https://github.com/stmr/emulsion") {
						log::error!("Could not open Emulsion website: {error}");
					}
				}
				action => ui.picture_widget.perform_action(action),
			},
		}
	}

	fn handle_about_to_wait(&mut self) -> NextUpdate {
		#[cfg(target_os = "macos")]
		if let Some(hide_at) = self.traffic_lights_hide_at {
			if Instant::now() >= hide_at {
				if let Some(ui) = &self.ui_elements {
					macos::set_traffic_lights_visible(&ui.window, false);
				}
				self.traffic_lights_visible = false;
				self.traffic_lights_hide_at = None;
				return NextUpdate::Latest;
			}
			return NextUpdate::WaitUntil(hide_at);
		}
		NextUpdate::Latest
	}

	fn should_exit_on_close(
		&mut self,
		_event_loop: &ActiveEventLoop,
		_window_id: gelatin::winit::window::WindowId,
	) -> bool {
		!cfg!(target_os = "macos")
			|| self
				.config
				.borrow()
				.window
				.as_ref()
				.and_then(|window| window.quit_on_close)
				.unwrap_or(false)
	}

	fn exiting(&mut self) {
		// Join only a finished check: it has stored its result in the cache by then, while one
		// still waiting on the network is abandoned rather than delaying the quit.
		if let Some(h) = self.update_checker_join_handle.take()
			&& h.is_finished()
			&& h.join().is_err()
		{
			log::error!("The update check thread panicked");
		}
		let cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
		if let Err(error) = cache.save(&self.cache_path) {
			log::error!("{error}");
		}
	}
}

fn make_icon() -> Icon {
	let img = image::load_from_memory(include_bytes!("../resource/emulsion48.png")).unwrap();
	let rgba = img.into_rgba8();
	let (w, h) = rgba.dimensions();
	Icon::from_rgba(rgba.into_raw(), w, h).unwrap()
}

fn add_window_movement_listener(window: &Window, cache: Arc<Mutex<Cache>>) {
	window.add_global_event_handler(move |window, event| match event {
		WindowEvent::Resized(new_size) => {
			let mut cache = cache.lock().unwrap();
			cache.window.win_w = new_size.width;
			cache.window.win_h = new_size.height;
			cache.window.maximized = window.window_mut().is_maximized();
		}
		WindowEvent::Moved(new_pos) => {
			let mut cache = cache.lock().unwrap();
			cache.window.win_x = new_pos.x;
			cache.window.win_y = new_pos.y;
			cache.window.maximized = window.window_mut().is_maximized();
		}
		_ => (),
	});
}

fn make_root_container() -> Rc<VerticalLayoutContainer> {
	let container = Rc::new(VerticalLayoutContainer::new());
	container.set_margin_all(0.0);
	container.set_height(Length::Stretch { min: 0.0, max: f32::INFINITY });
	container.set_width(Length::Stretch { min: 0.0, max: f32::INFINITY });
	container
}

fn make_picture_area_container() -> Rc<VerticalLayoutContainer> {
	let picture_area_container = Rc::new(VerticalLayoutContainer::new());
	picture_area_container.set_margin_all(0.0);
	picture_area_container.set_height(Length::Stretch { min: 0.0, max: f32::INFINITY });
	picture_area_container.set_width(Length::Stretch { min: 0.0, max: f32::INFINITY });
	picture_area_container
}

fn make_update_label() -> Rc<Label> {
	let update_label = Rc::new(Label::new());
	update_label.set_margin_top(4.0);
	update_label.set_margin_bottom(4.0);
	update_label.set_fixed_size(LogicalVector::new(200.0, 24.0));
	update_label.set_horizontal_align(Alignment::Center);
	update_label
}

fn make_update_notification(update_label: Rc<Label>) -> Rc<HorizontalLayoutContainer> {
	let container = Rc::new(HorizontalLayoutContainer::new());
	container.set_vertical_align(Alignment::End);
	container.set_horizontal_align(Alignment::Start);
	container.set_width(Length::Stretch { min: 0.0, max: f32::INFINITY });
	container.set_height(Length::Fixed(32.0));

	let update_button = Rc::new(Button::new());
	let button_image = Rc::new(Picture::from_encoded_bytes(VISIT_SITE));
	update_button.set_icon(Some(button_image));
	update_button.set_margin_top(4.0);
	update_button.set_margin_bottom(4.0);
	update_button.set_fixed_size(LogicalVector::new(100.0, 24.0));
	update_button.set_horizontal_align(Alignment::Center);
	update_button.set_on_click(|| {
		if let Err(error) = open::that("https://github.com/stmr/emulsion/releases/latest") {
			log::error!("Could not open the release page: {error}");
		}
	});

	container.add_child(update_label);
	container.add_child(update_button);
	container
}

fn make_picture_widget(
	window: &Rc<Window>,
	bottom_bar: Rc<BottomBar>,
	left_to_pan_hint: Rc<HelpScreen>,
	copy_notifications: CopyNotifications,
	config: Rc<RefCell<Configuration>>,
	cache: Arc<Mutex<Cache>>,
) -> Rc<PictureWidget> {
	let picture_widget = Rc::new(PictureWidget::new(
		&window.display_mut(),
		window,
		bottom_bar,
		left_to_pan_hint,
		copy_notifications,
		config,
		cache,
	));
	picture_widget.set_height(Length::Stretch { min: 0.0, max: f32::INFINITY });
	picture_widget.set_width(Length::Stretch { min: 0.0, max: f32::INFINITY });
	picture_widget
}

pub fn get_config_and_cache_paths() -> (PathBuf, PathBuf) {
	let (config_folder, cache_folder) = if let Some(project_dirs) = &*PROJECT_DIRS {
		(project_dirs.config_dir().to_owned(), project_dirs.cache_dir().to_owned())
	} else {
		let exe_folder = std::env::current_exe()
			.ok()
			.and_then(|exe_path| exe_path.parent().map(PathBuf::from))
			.unwrap_or_else(|| {
				log::error!("Could not locate the executable folder; using the working directory");
				PathBuf::from(".")
			});
		(exe_folder.clone(), exe_folder)
	};
	for folder in [&config_folder, &cache_folder] {
		if let Err(error) = std::fs::create_dir_all(folder) {
			log::error!("Could not create folder {folder:?}: {error}");
		}
	}

	(config_folder.join("cfg.toml"), cache_folder.join("cache.toml"))
}

/// Reports an unrecoverable startup failure. No window exists yet, so besides the log and
/// stderr a native dialog is shown where the process has no visible console (macOS, Windows).
fn fatal_startup_error(message: &str) -> ! {
	log::error!("{message}");
	eprintln!("Emulsion could not start: {message}");
	#[cfg(target_os = "macos")]
	macos::show_fatal_alert(message);
	#[cfg(windows)]
	show_fatal_message_box(message);
	std::process::exit(1)
}

#[cfg(windows)]
fn show_fatal_message_box(message: &str) {
	use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
	let wide = |text: &str| text.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
	// SAFETY: both strings are NUL-terminated and outlive the call; a null owner is allowed.
	unsafe {
		MessageBoxW(
			std::ptr::null_mut(),
			wide(message).as_ptr(),
			wide("Emulsion can't start").as_ptr(),
			MB_OK | MB_ICONERROR,
		);
	}
}

#[cfg(not(feature = "networking"))]
mod update {
	/// Always returns false without the `networking` feature.
	pub fn check_for_updates() -> bool {
		false
	}
}

#[cfg(feature = "networking")]
mod update {
	use serde::Deserialize;

	#[derive(Deserialize)]
	struct ReleaseInfoJson {
		tag_name: String,
	}

	type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

	/// Tries to fetch latest release tag
	fn latest_release() -> Result<ReleaseInfoJson> {
		let url = "https://api.github.com/repos/stmr/emulsion/releases/latest";
		// Without a global timeout an unreachable host blocks the check thread indefinitely.
		let agent: ureq::Agent = ureq::Agent::config_builder()
			.timeout_global(Some(std::time::Duration::from_secs(10)))
			.build()
			.into();
		let mut res = agent.get(url).header("User-Agent", "emulsion").call()?;
		Ok(res.body_mut().read_json()?)
	}

	/// Tries to parse version tag and compare against current version
	fn compare_release(info: &ReleaseInfoJson) -> Result<bool> {
		use crate::version::Version;
		use std::str::FromStr;

		let current = Version::cargo_pkg_version();
		let latest = Version::from_str(&info.tag_name)?;

		if latest > current {
			println!("Current version is {}, latest version is {}", current, latest);
			Ok(true)
		} else {
			Ok(false)
		}
	}

	/// Returns true if updates are available.
	pub fn check_for_updates() -> bool {
		match latest_release() {
			Ok(info) => match compare_release(&info) {
				Ok(is_newer) => is_newer,
				Err(err) => {
					eprintln!("Error parsing release tag: {}", err);
					false
				}
			},
			Err(err) => {
				eprintln!("Error checking latest release: {}", err);
				false
			}
		}
	}
}
