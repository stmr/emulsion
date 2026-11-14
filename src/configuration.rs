//! There are two files that store properties for Emulsion, the *cache* and the *config*.
//!
//! The most important distinction between these is that Emulsion never writes to the *config*
//! but it does write to the *cache* to save portions of the state of the program (e.g. window size
//! and position).
//!
//! Furthermore it's generally true that the user will only edit the *config* to specify their
//! preferences.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
pub const MIN_GAMMA: f32 = 0.25;
pub const MAX_GAMMA: f32 = 4.0;
pub const DEFAULT_GAMMA: f32 = 1.0;
pub const GAMMA_SLIDER_STEPS: u32 = 81;
const GAMMA_SLIDER_CENTER: f32 = (GAMMA_SLIDER_STEPS - 1) as f32 * 0.5;
const GAMMA_SLIDER_STEPS_PER_OCTAVE: f32 = 20.0;

pub fn clamp_gamma(gamma: f32) -> f32 {
	if gamma.is_nan() { DEFAULT_GAMMA } else { gamma.clamp(MIN_GAMMA, MAX_GAMMA) }
}

pub fn gamma_from_slider_position(position: u32) -> f32 {
	let position = position.min(GAMMA_SLIDER_STEPS - 1) as f32;
	clamp_gamma(2.0f32.powf((position - GAMMA_SLIDER_CENTER) / GAMMA_SLIDER_STEPS_PER_OCTAVE))
}

pub fn gamma_to_slider_position(gamma: f32) -> u32 {
	let gamma = clamp_gamma(gamma);
	((gamma.log2() * GAMMA_SLIDER_STEPS_PER_OCTAVE + GAMMA_SLIDER_CENTER).round() as i32)
		.clamp(0, GAMMA_SLIDER_STEPS as i32 - 1) as u32
}

#[cfg(test)]
mod gamma_tests {
	use super::*;

	#[test]
	fn gamma_slider_is_logarithmic_and_centered() {
		assert_eq!(gamma_from_slider_position(0), MIN_GAMMA);
		assert_eq!(gamma_from_slider_position(GAMMA_SLIDER_STEPS / 2), DEFAULT_GAMMA);
		assert_eq!(gamma_from_slider_position(GAMMA_SLIDER_STEPS - 1), MAX_GAMMA);
		assert!(gamma_from_slider_position(50) > gamma_from_slider_position(45));
	}

	#[test]
	fn gamma_values_are_clamped_and_round_trip_to_slider() {
		assert_eq!(clamp_gamma(-1.0), MIN_GAMMA);
		assert_eq!(clamp_gamma(10.0), MAX_GAMMA);
		assert_eq!(clamp_gamma(f32::NAN), DEFAULT_GAMMA);
		assert_eq!(gamma_to_slider_position(DEFAULT_GAMMA), GAMMA_SLIDER_STEPS / 2);
		assert_eq!(gamma_to_slider_position(MIN_GAMMA), 0);
		assert_eq!(gamma_to_slider_position(MAX_GAMMA), GAMMA_SLIDER_STEPS - 1);
	}
}

#[derive(Debug, Serialize, Deserialize, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
	Light,
	Dark,
}
impl Theme {
	pub fn switch_theme(self) -> Self {
		match self {
			Theme::Dark => Theme::Light,
			Theme::Light => Theme::Dark,
		}
	}
}

#[derive(Default, Debug, Copy, Clone, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Antialias {
	#[default]
	Auto,
	Always,
	Never,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheImageSection {
	/// New installs fill the window; `E` switches back to showing small images 1:1.
	pub fit_stretches: bool,
	pub antialiasing: Antialias,
}

impl Default for CacheImageSection {
	fn default() -> Self {
		Self { fit_stretches: true, antialiasing: Antialias::default() }
	}
}

#[derive(Debug, Default, PartialEq, Clone, Deserialize)]
pub struct ConfigImageSection {
	pub antialiasing: Option<String>,
	pub gamma: Option<f32>,
	pub reset_gamma_on_navigation: Option<bool>,
}

#[derive(Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheWindowSection {
	pub dark: bool,
	/// New installs follow the desktop appearance until the user picks a theme.
	pub follow_system_theme: bool,
	/// Persisted `off` / `auto` / `pinned` state of the thumbnail strip.
	pub thumbnails: String,
	pub win_w: u32,
	pub win_h: u32,
	pub win_x: i32,
	pub win_y: i32,
	pub maximized: bool,
}

fn default_thumbnails() -> String {
	"off".to_owned()
}

impl Default for CacheWindowSection {
	fn default() -> Self {
		Self {
			dark: false,
			follow_system_theme: true,
			thumbnails: default_thumbnails(),
			win_w: 580,
			win_h: 558,
			win_x: 64,
			win_y: 64,
			maximized: false,
		}
	}
}

#[derive(Debug, Clone, Deserialize)]
pub struct ConfigWindowSection {
	pub start_fullscreen: Option<bool>,
	pub quit_on_close: Option<bool>,
	pub start_maximized: Option<bool>,
	pub show_bottom_bar: Option<bool>,
	pub show_thumbnails: Option<String>,
	pub thumbnail_size: Option<f32>,
	pub theme: Option<Theme>,
	pub use_last_window_area: Option<bool>,
	pub win_w: Option<u32>,
	pub win_h: Option<u32>,
	pub win_x: Option<i32>,
	pub win_y: Option<i32>,
}

#[derive(Debug, Eq, PartialEq, Clone, Deserialize)]
#[serde(default)]
pub struct ConfigUpdateSection {
	pub check_updates: bool,
}
impl Default for ConfigUpdateSection {
	fn default() -> Self {
		Self { check_updates: true }
	}
}

#[derive(Default, Debug, Eq, PartialEq, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CacheUpdateSection {
	pub last_checked: u64,
}

impl CacheUpdateSection {
	pub fn update_check_needed(&self) -> bool {
		let duration = SystemTime::now()
			.duration_since(UNIX_EPOCH + Duration::from_secs(self.last_checked))
			.unwrap_or_else(|_| Duration::from_secs(0));

		duration > Duration::from_secs(60 * 60 * 24) // 24 hours
	}

	pub fn set_update_check_time(&mut self) {
		self.last_checked = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap_or_else(|_| Duration::from_secs(0))
			.as_secs();
	}
}

#[derive(Deserialize)]
struct IncompleteCache {
	pub window: Option<CacheWindowSection>,
	pub updates: Option<CacheUpdateSection>,
	pub image: Option<CacheImageSection>,
}

#[derive(Debug, Default, Eq, PartialEq, Clone, Serialize)]
pub struct Cache {
	pub window: CacheWindowSection,
	pub updates: CacheUpdateSection,
	pub image: CacheImageSection,
}
impl From<IncompleteCache> for Cache {
	fn from(cache: IncompleteCache) -> Self {
		Self {
			window: cache.window.unwrap_or_default(),
			updates: cache.updates.unwrap_or_default(),
			image: cache.image.unwrap_or_default(),
		}
	}
}
impl Cache {
	pub fn theme(&self) -> Theme {
		if self.window.dark { Theme::Dark } else { Theme::Light }
	}

	pub fn set_theme(&mut self, theme: Theme) {
		self.window.dark = theme == Theme::Dark;
	}

	pub fn load<P: AsRef<Path>>(file_path: P) -> Result<Cache, String> {
		let file_path = file_path.as_ref();
		let cfg_str = fs::read_to_string(file_path)
			.map_err(|e| format!("Could not read cache from {file_path:?}: {e}"))?;
		let result: IncompleteCache = toml::from_str(&cfg_str)
			.map_err(|e| format!("Could not parse cache {file_path:?}: {e}"))?;
		Ok(result.into())
	}

	/// Writes to a temporary sibling file first and renames it over the target, so a crash or
	/// full disk can never leave a truncated cache behind.
	pub fn save<P: AsRef<Path>>(&self, file_path: P) -> Result<(), String> {
		let file_path = file_path.as_ref();
		let string = toml::to_string(self).map_err(|e| format!("{e}"))?;
		let tmp_path = file_path.with_extension("toml.tmp");
		fs::write(&tmp_path, string).and_then(|()| fs::rename(&tmp_path, file_path)).map_err(|e| {
			let _ = fs::remove_file(&tmp_path);
			format!("Could not write cache file {file_path:?}: {e}")
		})
	}
}

#[derive(Debug, Default, Eq, PartialEq, Clone, Deserialize)]
pub struct EnvVar {
	pub name: String,
	pub value: String,
}

#[derive(Debug, Default, Eq, PartialEq, Clone, Deserialize)]
pub struct Command {
	pub input: Vec<String>,
	pub program: String,
	pub args: Option<Vec<String>>,
	pub envs: Option<Vec<EnvVar>>,
}

#[derive(Debug, Default, Eq, PartialEq, Clone, Deserialize)]
pub struct TitleSection {
	pub displayed_folders: Option<u32>,
	pub show_program_name: Option<bool>,
}
impl TitleSection {
	pub fn format_file_path<'a>(&self, file_path: &'a Path) -> Cow<'a, str> {
		match self.displayed_folders {
			Some(0) | None => file_path.file_name().unwrap().to_string_lossy(),
			Some(n) => {
				let mut component_count = 0;
				// On Windows the root can be the second component, when a `Prefix` is the first.
				let mut root_index = 0;
				for (idx, c) in file_path.components().enumerate() {
					component_count += 1;
					if c == std::path::Component::RootDir {
						root_index = idx as u32;
					}
				}

				if (component_count - root_index) <= (1 + n) {
					file_path.to_string_lossy().trim_start_matches("\\\\?\\").to_owned().into()
				} else {
					let ancestor = file_path.ancestors().take(2 + n as usize).last().unwrap();
					file_path.strip_prefix(ancestor).unwrap().to_string_lossy()
				}
			}
		}
	}

	pub fn format_program_name(&self) -> &'static str {
		match self.show_program_name {
			Some(false) => "",
			_ => " : E M U L S I O N",
		}
	}
}

#[derive(Debug, Default, Clone, Deserialize)]
pub struct Configuration {
	pub bindings: Option<BTreeMap<String, Vec<String>>>,
	pub commands: Option<Vec<Command>>,
	pub updates: Option<ConfigUpdateSection>,
	pub title: Option<TitleSection>,
	pub image: Option<ConfigImageSection>,
	pub window: Option<ConfigWindowSection>,
}
impl Configuration {
	pub fn load<P: AsRef<Path>>(file_path: P) -> Result<Configuration, String> {
		let file_path = file_path.as_ref();
		let cfg_str = fs::read_to_string(file_path)
			.map_err(|e| format!("Could not read config from {file_path:?}: {e}"))?;
		toml::from_str(&cfg_str).map_err(|e| format!("Could not parse config {file_path:?}: {e}"))
	}
}

#[cfg(test)]
mod load_tests {
	use super::*;

	#[test]
	fn partial_sections_fall_back_to_defaults() {
		let config: Configuration = toml::from_str("[updates]\n").unwrap();
		assert_eq!(config.updates, Some(ConfigUpdateSection { check_updates: true }));
		let cache: IncompleteCache = toml::from_str("[window]\ndark = true\n").unwrap();
		let cache = Cache::from(cache);
		assert!(cache.window.dark);
		assert_eq!(cache.window.win_w, CacheWindowSection::default().win_w);
	}

	#[test]
	fn save_replaces_cache_without_leftovers() {
		let dir = std::env::temp_dir().join(format!("emulsion-cache-test-{}", std::process::id()));
		fs::create_dir_all(&dir).unwrap();
		let path = dir.join("cache.toml");
		let mut cache = Cache::default();
		cache.window.win_w = 1234;
		cache.save(&path).unwrap();
		cache.window.win_w = 4321;
		cache.save(&path).unwrap();
		assert_eq!(Cache::load(&path).unwrap().window.win_w, 4321);
		assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
		fs::remove_dir_all(&dir).unwrap();
	}
}
