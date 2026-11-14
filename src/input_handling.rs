use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::process::Command;
use std::rc::Rc;
use std::sync::{LazyLock, Mutex};

use crate::configuration::Configuration;
use gelatin::winit::keyboard::ModifiersState;

pub static TOGGLE_FULLSCREEN_NAME: &str = "toggle_fullscreen";
pub static ESCAPE_NAME: &str = "escape";
pub static IMG_NEXT_NAME: &str = "img_next";
pub static IMG_PREV_NAME: &str = "img_prev";
pub static IMG_ORIG_NAME: &str = "img_orig";
pub static IMG_FIT_NAME: &str = "img_fit";
pub static IMG_FIT_BEST_NAME: &str = "img_fit_best";
pub static IMG_DEL_NAME: &str = "img_del";
pub static IMG_COPY_NAME: &str = "img_copy";
pub static PAN_NAME: &str = "pan";
pub static PAN_VERT_NAME: &str = "pan_vert"; // Vertical panning
pub static PAN_HOR_NAME: &str = "pan_hor"; // Horizontal panning
pub static PLAY_ANIM_NAME: &str = "play_anim";
pub static PLAY_PRESENT_NAME: &str = "play_present";
pub static PLAY_PRESENT_RND_NAME: &str = "play_present_rnd";
pub static TOGGLE_ANTIALIAS_NAME: &str = "toggle_antialias";
pub static SET_AUTOMATIC_ANTIALIAS_NAME: &str = "automatic_antialias";
pub static ZOOM_IN_NAME: &str = "zoom_in";
pub static ZOOM_OUT_NAME: &str = "zoom_out";
pub static PAN_LEFT_NAME: &str = "pan_left";
pub static PAN_RIGHT_NAME: &str = "pan_right";
pub static PAN_UP_NAME: &str = "pan_up";
pub static GAMMA_DECREASE_NAME: &str = "gamma_decrease";
pub static GAMMA_INCREASE_NAME: &str = "gamma_increase";
pub static GAMMA_RESET_NAME: &str = "gamma_reset";
pub static TOGGLE_THUMBNAILS_NAME: &str = "toggle_thumbnails";

pub static PAN_DOWN_NAME: &str = "pan_down";

pub static DEFAULT_BINDINGS: LazyLock<HashMap<&'static str, Vec<&'static str>>> =
	LazyLock::new(|| {
		let mut m = HashMap::new();
		m.insert(TOGGLE_FULLSCREEN_NAME, vec!["F11", "Return"]);
		m.insert(ESCAPE_NAME, vec!["Escape"]);
		// `BrowserForward`/`CmdCtrl+]` cover mice whose driver sends a keystroke
		// instead of the mouse button.
		m.insert(IMG_NEXT_NAME, vec!["D", "Right", "PageDown", "BrowserForward", "CmdCtrl+]"]);
		m.insert(IMG_PREV_NAME, vec!["A", "Left", "PageUp", "BrowserBack", "CmdCtrl+["]);
		m.insert(IMG_ORIG_NAME, vec!["Q", "1"]);
		m.insert(IMG_FIT_NAME, vec!["F"]);
		m.insert(IMG_FIT_BEST_NAME, vec!["E"]);
		m.insert(IMG_DEL_NAME, vec!["Delete"]);
		m.insert(IMG_COPY_NAME, vec!["CmdCtrl+C"]);
		m.insert(PAN_NAME, vec!["Space"]);
		m.insert(PLAY_ANIM_NAME, vec!["Alt+A", "Alt+V"]);
		m.insert(PLAY_PRESENT_NAME, vec!["P"]);
		// `subtract`/`add` are the `-` and `+` keys; `char_to_input_key` normalises them.
		m.insert(GAMMA_DECREASE_NAME, vec!["[", "Subtract"]);
		m.insert(GAMMA_INCREASE_NAME, vec!["]", "Add"]);
		m.insert(GAMMA_RESET_NAME, vec!["\\"]);
		m.insert(TOGGLE_THUMBNAILS_NAME, vec!["T"]);

		m.insert(PLAY_PRESENT_RND_NAME, vec!["Alt+P"]);
		m.insert(TOGGLE_ANTIALIAS_NAME, vec!["S"]);
		m.insert(SET_AUTOMATIC_ANTIALIAS_NAME, vec!["Alt+S"]);
		m
	});

pub fn char_to_input_key(ch: &str) -> String {
	let mut input_key = String::with_capacity(8);
	if ch == " " {
		input_key.push_str("space");
	} else if ch == "+" {
		input_key.push_str("add");
	} else if ch == "-" {
		input_key.push_str("subtract");
	} else {
		input_key.push_str(ch);
	}
	input_key
}

fn substitute_command_parameters(string: &str, var_map: &HashMap<&str, &str>) -> String {
	let mut result = String::from(string);
	for (&var_name, &substitute) in var_map.iter() {
		result = result.replace(var_name, substitute);
	}
	result
}

/// Execute all custom commands that were triggered by the input key and modifier set and
/// report whether any of them was triggered.
///
/// The commands run detached: a reaper thread waits on each child so the UI never blocks on
/// them.
///
/// Note: img_path and folder_path both have to be str instead of Path because we
/// wouldn't be able to construct a command from them if they cannot be converted to
/// valid UTF-8.
pub fn execute_triggered_commands(
	config: Rc<RefCell<Configuration>>,
	input_key: &str,
	modifiers: ModifiersState,
	img_path: &str,
	folder_path: &str,
) -> bool {
	let mut any_triggered = false;
	let config = config.borrow();
	if let Some(commands) = &config.commands {
		let mut var_map = HashMap::with_capacity(2);
		var_map.insert("${img}", img_path);
		var_map.insert("${folder}", folder_path);
		for command in commands.iter() {
			if !keys_triggered(&command.input, input_key, modifiers) {
				continue;
			}
			any_triggered = true;
			let mut cmd = Command::new(&command.program);
			if let Some(args) = &command.args {
				cmd.args(args.iter().map(|arg| substitute_command_parameters(arg, &var_map)));
			}
			if let Some(envs) = &command.envs {
				cmd.envs(
					envs.iter().map(|env_var| (env_var.name.as_str(), env_var.value.as_str())),
				);
			}
			match cmd.spawn() {
				Ok(mut child) => {
					let program = command.program.clone();
					std::thread::spawn(move || match child.wait() {
						Ok(status) if status.success() => {}
						Ok(status) => log::warn!("User command {program:?} exited with {status}"),
						Err(e) => {
							log::error!("Error while waiting for user command {program:?}: {e}")
						}
					});
				}
				Err(e) => log::error!(
					"Error while executing the following user command. See the error below.\n{command:?}\nError: {e:?}"
				),
			}
		}
	}
	any_triggered
}

/// A parsed binding like `Ctrl+Shift+X`. `shift` only matters when it was written out: a
/// bare binding also fires with Shift held, because Shift is part of producing characters such
/// as `?` on many layouts.
#[derive(Debug, PartialEq, Eq)]
struct KeyBinding {
	key: String,
	alt: bool,
	ctrl: bool,
	logo: bool,
	shift: bool,
}

/// Warns about a binding with a modifier we don't know, once per binding.
fn warn_unknown_modifier(binding: &str, modifier: &str) {
	static WARNED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
	if let Ok(mut warned) = WARNED.lock()
		&& warned.insert(binding.to_owned())
	{
		log::warn!("Ignoring the binding {binding:?}: unknown modifier {modifier:?}");
	}
}

/// Returns `None` if the binding uses an unknown modifier, in which case it never matches.
fn parse_binding(binding: &str) -> Option<KeyBinding> {
	let binding = binding.trim();
	// The key is whatever follows the last separator; a trailing `+` is the plus key itself,
	// as in `Ctrl++`.
	let (modifiers, key) = match binding.strip_suffix('+') {
		Some(rest) => (rest.strip_suffix('+').unwrap_or(rest), "+"),
		None => binding.rsplit_once('+').unwrap_or(("", binding)),
	};
	let mut parsed = KeyBinding {
		key: char_to_input_key(&key.trim().to_lowercase()),
		alt: false,
		ctrl: false,
		logo: false,
		shift: false,
	};
	for token in modifiers.split('+').map(str::trim).filter(|token| !token.is_empty()) {
		match token.to_lowercase().as_str() {
			"alt" | "option" => parsed.alt = true,
			"ctrl" | "control" => parsed.ctrl = true,
			"logo" | "cmd" | "command" | "super" | "win" => parsed.logo = true,
			"shift" => parsed.shift = true,
			"cmdctrl" => {
				if cfg!(target_os = "macos") {
					parsed.logo = true;
				} else {
					parsed.ctrl = true;
				}
			}
			_ => {
				warn_unknown_modifier(binding, token);
				return None;
			}
		}
	}
	Some(parsed)
}

pub fn keys_triggered<S: AsRef<str>>(
	keys: &[S],
	input_key: &str,
	modifiers: ModifiersState,
) -> bool {
	keys.iter().any(|binding| {
		parse_binding(binding.as_ref()).is_some_and(|binding| {
			!binding.key.is_empty()
				&& binding.key == input_key
				&& binding.alt == modifiers.alt_key()
				&& binding.ctrl == modifiers.control_key()
				&& binding.logo == modifiers.super_key()
				&& (!binding.shift || modifiers.shift_key())
		})
	})
}

pub fn action_triggered(
	config: &Rc<RefCell<Configuration>>,
	action_name: &str,
	input_key: &str,
	modifiers: ModifiersState,
) -> bool {
	let config = config.borrow();
	let bindings = config.bindings.as_ref();

	if let Some(Some(keys)) = bindings.map(|b| b.get(action_name)) {
		keys_triggered(keys.as_slice(), input_key, modifiers)
	} else {
		let empty = Vec::new();
		let keys = DEFAULT_BINDINGS.get(action_name).unwrap_or(&empty);
		keys_triggered(keys.as_slice(), input_key, modifiers)
	}
}
#[cfg(test)]
mod tests {
	use super::*;
	use std::collections::BTreeMap;

	#[test]
	fn gamma_defaults_use_the_agreed_key_mapping() {
		let modifiers = ModifiersState::empty();
		assert!(action_triggered(
			&Rc::new(RefCell::new(Configuration::default())),
			GAMMA_DECREASE_NAME,
			"[",
			modifiers,
		));
		assert!(action_triggered(
			&Rc::new(RefCell::new(Configuration::default())),
			GAMMA_INCREASE_NAME,
			"]",
			modifiers,
		));
		assert!(action_triggered(
			&Rc::new(RefCell::new(Configuration::default())),
			GAMMA_RESET_NAME,
			"\\",
			modifiers,
		));
	}

	#[test]
	fn gamma_also_binds_the_minus_and_plus_keys() {
		let config = Rc::new(RefCell::new(Configuration::default()));
		let bare = ModifiersState::empty();
		assert!(action_triggered(&config, GAMMA_DECREASE_NAME, "subtract", bare));
		assert!(action_triggered(&config, GAMMA_INCREASE_NAME, "add", bare));
	}

	#[test]
	fn browser_navigation_keys_map_to_image_navigation() {
		let config = Rc::new(RefCell::new(Configuration::default()));
		let modifiers = ModifiersState::empty();
		assert!(action_triggered(&config, IMG_PREV_NAME, "browserback", modifiers));
		assert!(action_triggered(&config, IMG_NEXT_NAME, "browserforward", modifiers));
	}

	#[test]
	fn driver_bracket_shortcuts_navigate_without_touching_gamma() {
		let config = Rc::new(RefCell::new(Configuration::default()));
		let with_cmd = ModifiersState::SUPER;
		let bare = ModifiersState::empty();
		assert!(action_triggered(&config, IMG_PREV_NAME, "[", with_cmd));
		assert!(action_triggered(&config, IMG_NEXT_NAME, "]", with_cmd));
		assert!(!action_triggered(&config, GAMMA_DECREASE_NAME, "[", with_cmd));
		assert!(action_triggered(&config, GAMMA_DECREASE_NAME, "[", bare));
		assert!(!action_triggered(&config, IMG_PREV_NAME, "[", bare));
	}

	#[test]
	fn user_binding_replaces_the_default_for_one_action() {
		let mut bindings = BTreeMap::new();
		bindings.insert(GAMMA_INCREASE_NAME.to_owned(), vec!["G".to_owned()]);
		let config = Rc::new(RefCell::new(Configuration {
			bindings: Some(bindings),
			..Configuration::default()
		}));
		let modifiers = ModifiersState::empty();
		assert!(action_triggered(&config, GAMMA_INCREASE_NAME, "g", modifiers));
		assert!(!action_triggered(&config, GAMMA_INCREASE_NAME, "]", modifiers));
		assert!(action_triggered(&config, GAMMA_DECREASE_NAME, "[", modifiers));
	}

	fn triggered(binding: &str, key: &str, modifiers: ModifiersState) -> bool {
		keys_triggered(&[binding], key, modifiers)
	}

	#[test]
	fn modifier_bindings_need_their_modifiers() {
		let none = ModifiersState::empty();
		assert!(triggered("X", "x", none));
		assert!(!triggered("Ctrl+X", "x", none));
		assert!(triggered("Ctrl+X", "x", ModifiersState::CONTROL));
		assert!(triggered("Control+X", "x", ModifiersState::CONTROL));
		assert!(!triggered("Ctrl+X", "x", ModifiersState::CONTROL | ModifiersState::ALT));
		assert!(triggered("Option+X", "x", ModifiersState::ALT));
		for name in ["Logo", "Cmd", "Command", "Super", "Win"] {
			assert!(triggered(&format!("{name}+X"), "x", ModifiersState::SUPER), "{name}");
			assert!(!triggered(&format!("{name}+X"), "x", none), "{name}");
		}
		assert!(triggered("Ctrl+Alt+X", "x", ModifiersState::CONTROL | ModifiersState::ALT));
		assert!(!triggered("Ctrl+Alt+X", "x", ModifiersState::CONTROL));
	}

	#[test]
	fn cmdctrl_is_cmd_on_macos_and_ctrl_elsewhere() {
		let platform =
			if cfg!(target_os = "macos") { ModifiersState::SUPER } else { ModifiersState::CONTROL };
		assert!(triggered("CmdCtrl+C", "c", platform));
		assert!(!triggered("CmdCtrl+C", "c", ModifiersState::empty()));
	}

	#[test]
	fn explicit_shift_is_required_but_bare_bindings_ignore_it() {
		let shift = ModifiersState::SHIFT;
		assert!(triggered("Shift+X", "x", shift));
		assert!(!triggered("Shift+X", "x", ModifiersState::empty()));
		assert!(triggered("D", "d", shift));
		assert!(triggered("D", "d", ModifiersState::empty()));
		assert!(!triggered("Ctrl+D", "d", shift));
	}

	#[test]
	fn unknown_modifier_never_matches() {
		assert!(!triggered("Hyper+X", "x", ModifiersState::empty()));
		assert!(!triggered("Hyper+X", "x", ModifiersState::CONTROL));
	}

	#[test]
	fn minus_and_plus_keys_can_be_bound_by_symbol() {
		let none = ModifiersState::empty();
		assert!(triggered("-", "subtract", none));
		assert!(triggered("+", "add", none));
		assert!(triggered("Ctrl++", "add", ModifiersState::CONTROL));
		assert!(triggered("Ctrl+-", "subtract", ModifiersState::CONTROL));
		assert!(!triggered("Ctrl++", "add", none));
		assert!(triggered("Ctrl+Add", "add", ModifiersState::CONTROL));
	}

	#[test]
	fn bindings_are_trimmed_and_case_insensitive() {
		assert!(triggered(" ctrl + X ", "x", ModifiersState::CONTROL));
		assert!(triggered("Space", "space", ModifiersState::empty()));
		assert!(!triggered("", "", ModifiersState::empty()));
	}
}
