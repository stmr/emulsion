use std::{fmt::Debug, rc::Rc};

use crate::{
	application::Application,
	window::{Window, WindowDescriptor},
};

use winit::event_loop::EventLoop as WinitEventLoop;

#[cfg(target_os = "macos")]
use winit::platform::macos::EventLoopBuilderExtMacOS;

pub struct EventLoop<UserEvent: Debug + 'static> {
	pub(crate) inner: winit::event_loop::EventLoop<UserEvent>,
}

impl<UserEvent> EventLoop<UserEvent>
where
	UserEvent: Debug + 'static,
{
	pub fn new() -> Self {
		let mut builder = WinitEventLoop::<UserEvent>::with_user_event();
		#[cfg(target_os = "macos")]
		builder.with_default_menu(false);
		Self { inner: builder.build().unwrap() }
	}

	pub fn create_proxy(&self) -> winit::event_loop::EventLoopProxy<UserEvent> {
		self.inner.create_proxy()
	}
}

impl<UserEvent> Default for EventLoop<UserEvent>
where
	UserEvent: Debug + 'static,
{
	fn default() -> Self {
		Self::new()
	}
}

pub struct ActiveEventLoop<'a> {
	pub(crate) inner: &'a winit::event_loop::ActiveEventLoop,
	pub(crate) application: &'a mut Application,
}

impl<'a> ActiveEventLoop<'a> {
	pub fn create_window(
		&mut self,
		desc: WindowDescriptor,
	) -> Result<Rc<Window>, Box<dyn std::error::Error>> {
		Window::new(self.application, desc, self.inner)
	}
}
