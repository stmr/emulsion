use super::picture_widget::ScalingMode;
use crate::{
	Configuration, Theme,
	configuration::{GAMMA_SLIDER_STEPS, clamp_gamma, gamma_to_slider_position},
};

use gelatin::{
	NextUpdate,
	button::Button,
	line_layout_container::HorizontalLayoutContainer,
	misc::{Alignment, Length},
	picture::Picture,
	slider::Slider,
};
use std::{
	cell::Cell,
	rc::Rc,
	time::{Duration, Instant},
};
static MOON: &[u8] = include_bytes!("../../resource/moon.png");
static LIGHT: &[u8] = include_bytes!("../../resource/light.png");
static QUESTION_BUTTON: &[u8] = include_bytes!("../../resource/question_button.png");
static QUESTION_BUTTON_LIGHT: &[u8] = include_bytes!("../../resource/question_button_light.png");
static QUESTION_NOTI: &[u8] = include_bytes!("../../resource/question-noti.png");
static QUESTION_LIGHT_NOTI: &[u8] = include_bytes!("../../resource/question-light-noti.png");
static ONE: &[u8] = include_bytes!("../../resource/1.png");
static ONE_LIGHT: &[u8] = include_bytes!("../../resource/1-light.png");
static FIT_STRETCH: &[u8] = include_bytes!("../../resource/fit-stretch.png");
static FIT_STRETCH_LIGHT: &[u8] = include_bytes!("../../resource/fit-stretch-light.png");
static FIT_BEST: &[u8] = include_bytes!("../../resource/fit-min.png");

const BOTTOM_EDGE_REVEAL_DISTANCE: f32 = 24.0;
const BOTTOM_BAR_HIDE_DELAY: Duration = Duration::from_millis(800);

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub enum VisibilityPolicy {
	Hidden,
	AutoHide,
	Pinned,
}

impl VisibilityPolicy {
	fn from_config(config: &Configuration) -> Self {
		match config.window.as_ref().and_then(|window| window.show_bottom_bar) {
			Some(false) => Self::Hidden,
			Some(true) => Self::Pinned,
			None => Self::AutoHide,
		}
	}
}

static FIT_BEST_LIGHT: &[u8] = include_bytes!("../../resource/fit-min-light.png");

const NO_BG_COLOR: [f32; 4] = [0.0, 0.0, 0.0, 0.0];
const ACTIVE_BG_COLOR: [f32; 4] = [0.3, 0.3, 0.3, 0.5];

const SMALL_BUTTON_GAP: f32 = 4.0;
const BIG_BUTTON_GAP: f32 = 32.0;
const BUTTON_SIZE: f32 = 24.0;
pub const BOTTOM_BAR_HEIGHT: f32 = 32.0;

pub struct BottomBar {
	pub widget: Rc<HorizontalLayoutContainer>,
	pub orig_scale_button: Rc<Button>,
	pub fit_stretch_button: Rc<Button>,
	pub fit_best_button: Rc<Button>,
	pub slider: Rc<Slider>,
	pub gamma_slider: Rc<Slider>,
	pub theme_button: Rc<Button>,
	pub help_button: Rc<Button>,

	pub visibility_policy: VisibilityPolicy,
	hide_at: Cell<Option<Instant>>,
	is_fullscreen: Cell<bool>,
	is_visible: Cell<bool>,

	question: Rc<Picture>,
	question_light: Rc<Picture>,
	question_noti: Rc<Picture>,
	question_light_noti: Rc<Picture>,
	moon_img: Rc<Picture>,
	light_img: Rc<Picture>,
	one: Rc<Picture>,
	one_light: Rc<Picture>,
	fit_stretch: Rc<Picture>,
	fit_stretch_light: Rc<Picture>,
	fit_best: Rc<Picture>,
	fit_best_light: Rc<Picture>,
}

impl BottomBar {
	pub fn new(config: &Configuration) -> Self {
		let question = Rc::new(Picture::from_encoded_bytes(QUESTION_BUTTON));
		let question_light = Rc::new(Picture::from_encoded_bytes(QUESTION_BUTTON_LIGHT));
		let question_noti = Rc::new(Picture::from_encoded_bytes(QUESTION_NOTI));
		let question_light_noti = Rc::new(Picture::from_encoded_bytes(QUESTION_LIGHT_NOTI));
		let moon_img = Rc::new(Picture::from_encoded_bytes(MOON));
		let light_img = Rc::new(Picture::from_encoded_bytes(LIGHT));
		let one = Rc::new(Picture::from_encoded_bytes(ONE));
		let one_light = Rc::new(Picture::from_encoded_bytes(ONE_LIGHT));
		let fit_stretch = Rc::new(Picture::from_encoded_bytes(FIT_STRETCH));
		let fit_stretch_light = Rc::new(Picture::from_encoded_bytes(FIT_STRETCH_LIGHT));
		let fit_best = Rc::new(Picture::from_encoded_bytes(FIT_BEST));
		let fit_best_light = Rc::new(Picture::from_encoded_bytes(FIT_BEST_LIGHT));

		let widget = Rc::new(HorizontalLayoutContainer::new());
		widget.set_margin_left(0.0);
		widget.set_margin_right(0.0);
		widget.set_height(Length::Fixed(32.0));
		widget.set_width(Length::Stretch { min: 0.0, max: f32::INFINITY });
		widget.set_ignore_layout(true);
		widget.set_vertical_align(Alignment::End);

		let orig_scale_button = make_icon_button(Alignment::Start);
		let fit_best_button = make_icon_button(Alignment::Start);
		let fit_stretch_button = make_icon_button(Alignment::Start);
		let slider = make_slider();
		let gamma_slider = make_gamma_slider();
		let theme_button = make_icon_button(Alignment::End);
		let help_button = make_icon_button(Alignment::End);

		orig_scale_button.set_margin_left(SMALL_BUTTON_GAP);
		fit_stretch_button.set_margin_right(SMALL_BUTTON_GAP);
		gamma_slider.set_margin_right(SMALL_BUTTON_GAP);
		theme_button.set_margin_left(SMALL_BUTTON_GAP);
		help_button.set_margin_left(SMALL_BUTTON_GAP);
		help_button.set_margin_right(SMALL_BUTTON_GAP);

		if let Some(gamma) = config.image.as_ref().and_then(|image| image.gamma) {
			gamma_slider.set_value(gamma_to_slider_position(clamp_gamma(gamma)));
		}

		widget.add_child(orig_scale_button.clone());
		widget.add_child(fit_best_button.clone());
		widget.add_child(fit_stretch_button.clone());
		widget.add_child(gamma_slider.clone());
		widget.add_child(slider.clone());
		widget.add_child(theme_button.clone());
		widget.add_child(help_button.clone());

		let visibility_policy = VisibilityPolicy::from_config(config);
		let is_visible = visibility_policy == VisibilityPolicy::Pinned;
		widget.set_visible(is_visible);

		Self {
			widget,
			orig_scale_button,
			fit_stretch_button,
			fit_best_button,
			slider,
			gamma_slider,
			theme_button,
			help_button,
			visibility_policy,
			hide_at: Cell::new(None),
			is_fullscreen: Cell::new(false),
			is_visible: Cell::new(is_visible),

			question,
			question_light,
			question_noti,
			question_light_noti,
			moon_img,
			light_img,
			one,
			one_light,
			fit_stretch,
			fit_stretch_light,
			fit_best,
			fit_best_light,
		}
	}

	pub fn set_theme(&self, theme: Theme, update_available: bool) {
		match theme {
			Theme::Light => {
				self.orig_scale_button.set_icon(Some(self.one.clone()));
				self.fit_best_button.set_icon(Some(self.fit_best.clone()));
				self.fit_stretch_button.set_icon(Some(self.fit_stretch.clone()));
				self.theme_button.set_icon(Some(self.moon_img.clone()));
				self.widget.set_bg_color([1.0, 1.0, 1.0, 1.0]);
				self.slider.set_shadow_color([0.0, 0.0, 0.0]);

				if update_available {
					self.help_button.set_icon(Some(self.question_noti.clone()));
				} else {
					self.help_button.set_icon(Some(self.question.clone()));
				}
			}
			Theme::Dark => {
				self.orig_scale_button.set_icon(Some(self.one_light.clone()));
				self.fit_best_button.set_icon(Some(self.fit_best_light.clone()));
				self.fit_stretch_button.set_icon(Some(self.fit_stretch_light.clone()));
				self.theme_button.set_icon(Some(self.light_img.clone()));
				self.widget.set_bg_color([0.08, 0.08, 0.08, 1.0]);
				self.slider.set_shadow_color([0.0, 0.0, 0.0]);

				if update_available {
					self.help_button.set_icon(Some(self.question_light_noti.clone()));
				} else {
					self.help_button.set_icon(Some(self.question_light.clone()));
				}
			}
		}
	}

	pub fn set_gamma_slider_value(&self, gamma: f32) {
		self.gamma_slider.set_value(gamma_to_slider_position(clamp_gamma(gamma)));
	}

	pub fn set_fullscreen(&self, fullscreen: bool) {
		self.is_fullscreen.set(fullscreen);
		self.hide_at.set(None);
		match self.visibility_policy {
			VisibilityPolicy::Hidden | VisibilityPolicy::AutoHide if fullscreen => {
				self.set_widget_visible(false);
			}
			VisibilityPolicy::Hidden | VisibilityPolicy::AutoHide => {
				self.set_widget_visible(false);
			}
			VisibilityPolicy::Pinned => {
				self.set_widget_visible(!fullscreen);
			}
		}
	}

	pub fn handle_pointer_move(&self, cursor_y: f32, window_height: f32) {
		let at_bottom_edge = cursor_y >= (window_height - BOTTOM_EDGE_REVEAL_DISTANCE).max(0.0);
		match self.visibility_policy {
			VisibilityPolicy::Hidden | VisibilityPolicy::Pinned => {
				self.hide_at.set(None);
			}
			VisibilityPolicy::AutoHide if self.is_fullscreen.get() => {
				self.hide_at.set(None);
				self.set_widget_visible(false);
			}
			VisibilityPolicy::AutoHide if at_bottom_edge => {
				self.hide_at.set(None);
				self.set_widget_visible(true);
			}
			VisibilityPolicy::AutoHide => {
				if self.is_visible.get() {
					self.hide_at.set(Some(Instant::now() + BOTTOM_BAR_HIDE_DELAY));
				}
			}
		}
	}

	pub fn is_bar_visible(&self) -> bool {
		self.is_visible.get()
	}

	pub fn before_draw(&self) -> NextUpdate {
		if self.visibility_policy != VisibilityPolicy::AutoHide
			|| self.is_fullscreen.get()
			|| !self.is_visible.get()
		{
			return NextUpdate::Latest;
		}
		if let Some(hide_at) = self.hide_at.get() {
			let now = Instant::now();
			if now >= hide_at {
				self.hide_at.set(None);
				self.set_widget_visible(false);
				NextUpdate::Latest
			} else {
				NextUpdate::WaitUntil(hide_at)
			}
		} else {
			NextUpdate::Latest
		}
	}

	fn set_widget_visible(&self, visible: bool) {
		if self.is_visible.replace(visible) != visible {
			self.widget.set_visible(visible);
		}
	}

	pub fn set_help_visible(&self, visible: bool) {
		self.help_button.set_bg_color(if visible { ACTIVE_BG_COLOR } else { NO_BG_COLOR })
	}

	pub fn update_scaling_buttons(&self, scaling: ScalingMode, img_texel_size: f32) {
		match scaling {
			#[allow(clippy::float_cmp)]
			ScalingMode::Fixed => {
				if img_texel_size == 1.0 {
					self.orig_scale_button.set_bg_color(ACTIVE_BG_COLOR);
				} else {
					self.orig_scale_button.set_bg_color(NO_BG_COLOR);
				}
				self.fit_best_button.set_bg_color(NO_BG_COLOR);
				self.fit_stretch_button.set_bg_color(NO_BG_COLOR);
			}
			ScalingMode::FitMin => {
				self.orig_scale_button.set_bg_color(NO_BG_COLOR);
				self.fit_best_button.set_bg_color(ACTIVE_BG_COLOR);
				self.fit_stretch_button.set_bg_color(NO_BG_COLOR);
			}
			ScalingMode::FitStretch => {
				self.orig_scale_button.set_bg_color(NO_BG_COLOR);
				self.fit_best_button.set_bg_color(NO_BG_COLOR);
				self.fit_stretch_button.set_bg_color(ACTIVE_BG_COLOR);
			}
		}
	}
}

fn make_icon_button(alignment: Alignment) -> Rc<Button> {
	let button = Rc::new(Button::new());
	button.set_margin_top(SMALL_BUTTON_GAP);
	button.set_height(Length::Fixed(BUTTON_SIZE));
	button.set_width(Length::Fixed(BUTTON_SIZE));
	button.set_horizontal_align(alignment);
	button
}

fn make_slider() -> Rc<Slider> {
	let slider = Rc::new(Slider::new());
	slider.set_margin_top(SMALL_BUTTON_GAP);
	slider.set_margin_left(BIG_BUTTON_GAP);
	slider.set_margin_right(BIG_BUTTON_GAP);
	slider.set_height(Length::Fixed(BUTTON_SIZE));
	slider.set_width(Length::Stretch { min: 0.0, max: f32::INFINITY });
	slider.set_horizontal_align(Alignment::Center);
	slider.set_steps(6, 1);
	slider
}

fn make_gamma_slider() -> Rc<Slider> {
	let slider = Rc::new(Slider::new());
	slider.set_margin_top(SMALL_BUTTON_GAP);
	slider.set_width(Length::Fixed(128.0));
	slider.set_height(Length::Fixed(BUTTON_SIZE));
	slider.set_horizontal_align(Alignment::Start);
	slider.set_steps(GAMMA_SLIDER_STEPS, GAMMA_SLIDER_STEPS / 2);
	slider
}
