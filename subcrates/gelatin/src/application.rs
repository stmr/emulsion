use std::{
	collections::hash_map::HashMap,
	fmt::Debug,
	marker::PhantomData,
	rc::Rc,
	sync::atomic::{AtomicBool, Ordering},
	time::{Duration, Instant},
};

use winit::{
	application::ApplicationHandler as WinitApplicationHandler,
	event::{self, WindowEvent},
	event_loop::{ActiveEventLoop as WinitActiveEventLoop, ControlFlow},
	window::WindowId,
};

use crate::{
	NextUpdate,
	event_loop::{ActiveEventLoop, EventLoop},
	window::Window,
};

static EXIT_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn request_exit() {
	EXIT_REQUESTED.store(true, Ordering::Relaxed);
}

fn set_control_flow(event_loop: &WinitActiveEventLoop, control_flow: ControlFlow) {
	if let ControlFlow::WaitUntil(time) = control_flow {
		let very_short_time_from_now = Instant::now() + Duration::from_micros(100);
		if time < very_short_time_from_now {
			event_loop.set_control_flow(ControlFlow::Poll);
		} else {
			event_loop.set_control_flow(control_flow);
		}
	} else {
		event_loop.set_control_flow(control_flow);
	}
}

/// Returns true if original was replaced by new
fn aggregate_control_flow(event_loop: &WinitActiveEventLoop, new: ControlFlow) -> bool {
	let original = event_loop.control_flow();
	match new {
		ControlFlow::Poll => {
			set_control_flow(event_loop, new);
			return true;
		}
		ControlFlow::WaitUntil(new_time) => match original {
			ControlFlow::WaitUntil(orig_time) => {
				if new_time < orig_time {
					set_control_flow(event_loop, new);
					return true;
				}
			}
			ControlFlow::Wait => {
				set_control_flow(event_loop, new);
				return true;
			}
			_ => (),
		},
		_ => (),
	}
	false
}

/// On Windows, there's a bug that causes the event loop to get stuck in a
/// repeated "Poll-like" loop with the control flow set to WaitUntil but the
/// loops keeps running iterations immediately after each other.
///
/// We use this function to check if the time specified in WaitUntil has already
/// been passed and if it is, then we change the control flow, to get "un-stuck"
fn sanitize_control_flow(event_loop: &WinitActiveEventLoop) {
	set_control_flow(event_loop, event_loop.control_flow());
}

pub struct Application {
	windows: HashMap<WindowId, Rc<Window>>,
	first_resume_done: bool,
}

impl Application {
	pub fn new() -> Self {
		Application { windows: HashMap::new(), first_resume_done: false }
	}

	pub(crate) fn register_window(&mut self, window: Rc<Window>) {
		self.windows.insert(window.get_id(), window);
	}

	pub fn start_event_loop<UserEvent: Debug + 'static>(
		&mut self,
		application_handler: impl ApplicationHandler<UserEvent>,
		event_loop: EventLoop<UserEvent>,
	) {
		#[cfg(feature = "benchmark")]
		let update_draw_dt = {
			let mut last_draw_time = std::time::Instant::now();
			let mut prev_draw_dts = vec![0f32; 64];
			let mut prev_draw_dt_index = 0;

			move || {
				let now = std::time::Instant::now();
				let delta_time = now.duration_since(last_draw_time).as_secs_f32();
				last_draw_time = now;
				prev_draw_dts[prev_draw_dt_index] = delta_time;
				prev_draw_dt_index = (prev_draw_dt_index + 1) % prev_draw_dts.len();
				if prev_draw_dt_index == 0 {
					let max_dt = prev_draw_dts.iter().fold(0.0f32, |a, &b| a.max(b));
					println!(
						"{} redraws finsished, max delta time in that duration was: {}ms, {} FPS",
						prev_draw_dts.len(),
						(max_dt * 1000.0).round() as i32,
						(1.0 / max_dt).round() as i32
					);
				}
			}
		};

		let mut app_with_app_handler = AppWithAppHandler {
			application: self,
			application_handler,
			user_event: PhantomData,
			#[cfg(feature = "benchmark")]
			update_draw_dt: Box::new(update_draw_dt),
		};

		event_loop.inner.run_app(&mut app_with_app_handler).unwrap();
	}
}

impl Default for Application {
	fn default() -> Self {
		Self::new()
	}
}

struct AppWithAppHandler<'a, UserEvent, AppHandler>
where
	UserEvent: Debug + 'static,
	AppHandler: ApplicationHandler<UserEvent>,
{
	application: &'a mut Application,
	application_handler: AppHandler,
	user_event: PhantomData<UserEvent>,
	#[cfg(feature = "benchmark")]
	update_draw_dt: Box<dyn FnMut() + 'a>,
}

pub trait ApplicationHandler<UserEvent: Debug + 'static = ()> {
	fn handle_can_create_surface(&mut self, event_loop: &mut ActiveEventLoop);
	fn handle_window_event(
		&mut self,
		event_loop: &ActiveEventLoop,
		window_id: WindowId,
		event: &WindowEvent,
	) -> NextUpdate;

	/// Handle an event sent through [`EventLoop::create_proxy`].
	///
	/// The default implementation intentionally does nothing.
	fn handle_user_event(&mut self, _event: UserEvent) {}
	fn handle_about_to_wait(&mut self) -> NextUpdate {
		NextUpdate::Latest
	}

	/// Decide whether a close request should terminate the application.
	///
	/// Returning `false` leaves the request to the application, which can
	/// hide or otherwise manage the window itself. The default preserves
	/// gelatin's historical exit-on-close behavior.
	fn should_exit_on_close(
		&mut self,
		_event_loop: &ActiveEventLoop,
		_window_id: WindowId,
	) -> bool {
		true
	}

	fn exiting(&mut self);
}

impl<'a, UserEvent, AppHandler> WinitApplicationHandler<UserEvent>
	for AppWithAppHandler<'a, UserEvent, AppHandler>
where
	UserEvent: Debug + 'static,
	AppHandler: ApplicationHandler<UserEvent>,
{
	fn resumed(&mut self, event_loop: &WinitActiveEventLoop) {
		if !self.application.first_resume_done {
			self.application.first_resume_done = true;
			let mut active_event_loop =
				ActiveEventLoop { inner: event_loop, application: self.application };
			self.application_handler.handle_can_create_surface(&mut active_event_loop);
		}
	}

	fn user_event(&mut self, _event_loop: &WinitActiveEventLoop, event: UserEvent) {
		self.application_handler.handle_user_event(event);
	}

	fn about_to_wait(&mut self, event_loop: &WinitActiveEventLoop) {
		if EXIT_REQUESTED.load(Ordering::Relaxed) {
			event_loop.exit();
			return;
		}
		let app_next_update = self.application_handler.handle_about_to_wait();
		aggregate_control_flow(event_loop, app_next_update.into());
		for window in self.application.windows.values() {
			window.main_events_cleared();
			if window.redraw_needed() {
				window.request_redraw();
			}
		}
	}

	fn exiting(&mut self, _event_loop: &WinitActiveEventLoop) {
		self.application_handler.exiting();
	}

	fn new_events(&mut self, event_loop: &WinitActiveEventLoop, _cause: event::StartCause) {
		event_loop.set_control_flow(ControlFlow::Wait);
		for window in self.application.windows.values() {
			let new_control_flow = window.handle_loop_wake_up().into();
			aggregate_control_flow(event_loop, new_control_flow);
		}
	}

	fn window_event(
		&mut self,
		event_loop: &WinitActiveEventLoop,
		window_id: WindowId,
		event: WindowEvent,
	) {
		sanitize_control_flow(event_loop);

		let handler_next_update = self.application_handler.handle_window_event(
			&ActiveEventLoop { inner: event_loop, application: self.application },
			window_id,
			&event,
		);
		aggregate_control_flow(event_loop, handler_next_update.into());

		if let WindowEvent::RedrawRequested = event {
			let new_control_flow =
				self.application.windows.get(&window_id).unwrap().redraw().into();
			aggregate_control_flow(event_loop, new_control_flow);
			#[cfg(feature = "benchmark")]
			(self.update_draw_dt)();
		}
		if matches!(event, WindowEvent::CloseRequested)
			&& self.application_handler.should_exit_on_close(
				&ActiveEventLoop { inner: event_loop, application: self.application },
				window_id,
			) {
			request_exit();
		}
		let destroyed;
		if let WindowEvent::Destroyed = event {
			destroyed = true;
		} else {
			destroyed = false;
		}
		self.application.windows.get(&window_id).unwrap().process_event::<UserEvent>(event);
		if destroyed {
			self.application.windows.remove(&window_id);
		}
	}
}
