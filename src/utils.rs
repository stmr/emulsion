use gelatin::winit::keyboard::{Key, KeyCode, NamedKey, PhysicalKey};

/// Name of the key at that physical position on a US layout.
///
/// Bindings are written with US key names, but the logical key depends on the active layout:
/// on a Czech layout the `[` key reports `ú`, so matching only the logical key would silently
/// drop those bindings. This is the layout-independent fallback.
pub fn physical_key_to_string(key: &PhysicalKey) -> Option<&'static str> {
	let PhysicalKey::Code(code) = key else {
		return None;
	};
	Some(match code {
		KeyCode::KeyA => "a",
		KeyCode::KeyB => "b",
		KeyCode::KeyC => "c",
		KeyCode::KeyD => "d",
		KeyCode::KeyE => "e",
		KeyCode::KeyF => "f",
		KeyCode::KeyG => "g",
		KeyCode::KeyH => "h",
		KeyCode::KeyI => "i",
		KeyCode::KeyJ => "j",
		KeyCode::KeyK => "k",
		KeyCode::KeyL => "l",
		KeyCode::KeyM => "m",
		KeyCode::KeyN => "n",
		KeyCode::KeyO => "o",
		KeyCode::KeyP => "p",
		KeyCode::KeyQ => "q",
		KeyCode::KeyR => "r",
		KeyCode::KeyS => "s",
		KeyCode::KeyT => "t",
		KeyCode::KeyU => "u",
		KeyCode::KeyV => "v",
		KeyCode::KeyW => "w",
		KeyCode::KeyX => "x",
		KeyCode::KeyY => "y",
		KeyCode::KeyZ => "z",
		KeyCode::Digit0 => "0",
		KeyCode::Digit1 => "1",
		KeyCode::Digit2 => "2",
		KeyCode::Digit3 => "3",
		KeyCode::Digit4 => "4",
		KeyCode::Digit5 => "5",
		KeyCode::Digit6 => "6",
		KeyCode::Digit7 => "7",
		KeyCode::Digit8 => "8",
		KeyCode::Digit9 => "9",
		KeyCode::BracketLeft => "[",
		KeyCode::BracketRight => "]",
		KeyCode::Backslash => "\\",
		KeyCode::Minus => "subtract",
		KeyCode::Equal => "add",
		KeyCode::Comma => ",",
		KeyCode::Period => ".",
		KeyCode::Slash => "/",
		KeyCode::Semicolon => ";",
		KeyCode::Quote => "'",
		KeyCode::Backquote => "`",
		_ => return None,
	})
}

/// Returns the textual name of the key as written in the config file.
///
/// (Right side, eg "Return" is the string that should be used in the config file)
pub fn virtual_keycode_to_string(key: &Key) -> String {
	match key {
		Key::Unidentified(_) => Default::default(),
		Key::Character(ch) => ch.to_string(),
		Key::Dead(ch) => ch.map_or(Default::default(), |ch| ch.into()),
		// Config names are the `NamedKey` variant names, except for these.
		Key::Named(named_key) => match named_key {
			NamedKey::Enter => "Return".into(),
			NamedKey::ArrowDown => "Down".into(),
			NamedKey::ArrowLeft => "Left".into(),
			NamedKey::ArrowRight => "Right".into(),
			NamedKey::ArrowUp => "Up".into(),
			key => format!("{key:?}"),
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn physical_positions_resolve_to_us_binding_names() {
		assert_eq!(physical_key_to_string(&PhysicalKey::Code(KeyCode::BracketLeft)), Some("["));
		assert_eq!(physical_key_to_string(&PhysicalKey::Code(KeyCode::BracketRight)), Some("]"));
		assert_eq!(physical_key_to_string(&PhysicalKey::Code(KeyCode::Backslash)), Some("\\"));
		assert_eq!(physical_key_to_string(&PhysicalKey::Code(KeyCode::KeyD)), Some("d"));
		assert_eq!(physical_key_to_string(&PhysicalKey::Code(KeyCode::F5)), None);
	}

	/// Config names are the variant names, apart from the historical arrow/Return spellings.
	#[test]
	fn named_keys_use_config_names() {
		let name = |key| virtual_keycode_to_string(&Key::Named(key));
		assert_eq!(name(NamedKey::Enter), "Return");
		assert_eq!(name(NamedKey::ArrowLeft), "Left");
		assert_eq!(name(NamedKey::BrowserForward), "BrowserForward");
		assert_eq!(name(NamedKey::F11), "F11");
		assert_eq!(name(NamedKey::TVInputHDMI1), "TVInputHDMI1");
	}
}
