use std::{ffi::CString, path::PathBuf, sync::OnceLock};

use gelatin::{window::Window, winit::event_loop::EventLoopProxy};
use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu, accelerator::Accelerator};
use objc2::{
	Encode, ffi,
	rc::Retained,
	runtime::{AnyClass, AnyObject, Bool, Imp, Sel},
	sel,
};
use objc2_app_kit::{
	NSAlert, NSApplication, NSColorSpace, NSModalResponseOK, NSOpenPanel, NSView, NSWindow,
	NSWindowButton,
};
use objc2_foundation::{MainThreadMarker, NSArray, NSString};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

use crate::EmulsionEvent;

static EVENT_PROXY: OnceLock<EventLoopProxy<EmulsionEvent>> = OnceLock::new();

/// `NSApplication` installs its own `odoc`/`rapp` Apple Event handlers while launching and
/// forwards them to its delegate, so the delegate - winit's - is where the handling has to
/// live. Without these methods LaunchServices reports "Emulsion cannot open files in the
/// Image format" and never delivers the path.
extern "C-unwind" fn delegate_open_file(
	_this: &AnyObject,
	_cmd: Sel,
	_sender: &AnyObject,
	filename: &NSString,
) -> Bool {
	let path = PathBuf::from(filename.to_string());
	log::debug!("Opening {path:?} requested by the system");
	match EVENT_PROXY.get() {
		Some(proxy) => Bool::new(proxy.send_event(EmulsionEvent::OpenPaths(vec![path])).is_ok()),
		None => Bool::NO,
	}
}

extern "C-unwind" fn delegate_should_handle_reopen(
	_this: &AnyObject,
	_cmd: Sel,
	_sender: &AnyObject,
	_has_visible_windows: Bool,
) -> Bool {
	if let Some(proxy) = EVENT_PROXY.get() {
		let _ = proxy.send_event(EmulsionEvent::Reopen);
	}
	Bool::YES
}

pub struct MacIntegration {
	_menu: Menu,
}

/// Adds the document handling methods to winit's already registered delegate class.
fn extend_application_delegate() {
	let Some(class) = AnyClass::get(c"WinitApplicationDelegate") else {
		log::error!("Delegate class missing; the system cannot hand images to Emulsion");
		return;
	};
	let object = <*mut AnyObject>::ENCODING.to_string();
	let boolean = Bool::ENCODING.to_string();
	let open_types = CString::new(format!("{boolean}@:{object}{object}")).unwrap();
	let reopen_types = CString::new(format!("{boolean}@:{object}{boolean}")).unwrap();
	let added = unsafe {
		(
			ffi::class_addMethod(
				(class as *const AnyClass).cast_mut(),
				sel!(application:openFile:),
				std::mem::transmute::<
					extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, &NSString) -> Bool,
					Imp,
				>(delegate_open_file),
				open_types.as_ptr(),
			),
			ffi::class_addMethod(
				(class as *const AnyClass).cast_mut(),
				sel!(applicationShouldHandleReopen:hasVisibleWindows:),
				std::mem::transmute::<
					extern "C-unwind" fn(&AnyObject, Sel, &AnyObject, Bool) -> Bool,
					Imp,
				>(delegate_should_handle_reopen),
				reopen_types.as_ptr(),
			),
		)
	};
	if !added.0.as_bool() {
		log::error!("Could not add application:openFile: to the app delegate");
	}
	if !added.1.as_bool() {
		log::error!("Could not add applicationShouldHandleReopen: to the app delegate");
	}
}

pub fn install(proxy: EventLoopProxy<EmulsionEvent>) -> MacIntegration {
	EVENT_PROXY.set(proxy.clone()).expect("macOS integration initialized more than once");
	extend_application_delegate();

	let (menu, window_menu, help_menu) = build_menu();
	menu.init_for_nsapp();
	window_menu.set_as_windows_menu_for_nsapp();
	help_menu.set_as_help_menu_for_nsapp();
	MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
		let _ = proxy.send_event(EmulsionEvent::Command(event.id.0));
	}));

	MacIntegration { _menu: menu }
}

pub fn choose_image() -> Option<PathBuf> {
	let mtm = MainThreadMarker::new()?;
	let extensions = [
		"jpg", "jpeg", "png", "apng", "gif", "webp", "tif", "tiff", "bmp", "avif", "svg", "tga",
		"targa", "ico", "hdr", "pbm", "pgm", "ppm", "pam", "heic", "heif", "hif", "cr2", "cr3",
		"nef", "arw", "dng", "raf", "orf", "rw2", "pef", "srw",
	]
	.map(NSString::from_str);
	let allowed_types = NSArray::from_retained_slice(&extensions);
	let panel = NSOpenPanel::openPanel(mtm);
	panel.setCanChooseDirectories(false);
	panel.setCanChooseFiles(true);
	panel.setAllowsMultipleSelection(false);
	// ponytail: extension filter; switch to setAllowedContentTypes when a UTType
	// binding is already in the dependency graph.
	#[allow(deprecated)]
	panel.setAllowedFileTypes(Some(&allowed_types));
	if panel.runModal() == NSModalResponseOK {
		panel.URL().and_then(|url| url.path()).map(|path| PathBuf::from(path.to_string()))
	} else {
		None
	}
}

/// Modal alert for failures before any window exists. Does nothing off the main thread.
pub fn show_fatal_alert(message: &str) {
	let Some(mtm) = MainThreadMarker::new() else { return };
	// An alert needs the shared application to exist; this is a no-op when winit already made it.
	let _ = NSApplication::sharedApplication(mtm);
	let alert = NSAlert::new(mtm);
	alert.setMessageText(&NSString::from_str("Emulsion can't start"));
	alert.setInformativeText(&NSString::from_str(message));
	alert.runModal();
}

fn ns_window(window: &Window) -> Option<Retained<NSWindow>> {
	let window_ref = window.window_mut();
	let handle = window_ref.window_handle().ok()?;
	let RawWindowHandle::AppKit(handle) = handle.as_raw() else {
		return None;
	};
	let ns_view = unsafe { &*handle.ns_view.as_ptr().cast::<NSView>() };
	ns_view.window()
}

/// The GL surface is untagged, so AppKit would show its pixels in the display's native gamut,
/// which is not what they hold. Tagged as Display P3, the pixels are colour matched to the
/// actual display (sRGB monitors included). Display P3 shares the sRGB transfer curve, so the
/// sRGB textures and framebuffer stay as they are; sRGB pictures are converted to the P3
/// primaries by the fragment shader (`image_loader::Primaries`).
pub fn use_display_p3_color_space(window: &Window) {
	if let Some(ns_window) = ns_window(window) {
		ns_window.setColorSpace(Some(&NSColorSpace::displayP3ColorSpace()));
	}
}

pub fn set_traffic_lights_visible(window: &Window, visible: bool) {
	let Some(ns_window) = ns_window(window) else {
		return;
	};
	for button in
		[NSWindowButton::CloseButton, NSWindowButton::MiniaturizeButton, NSWindowButton::ZoomButton]
	{
		if let Some(button) = ns_window.standardWindowButton(button) {
			button.setHidden(!visible);
		}
	}
}

/// Menu items that mirror a configurable key binding carry no key equivalent: the binding is
/// handled by the viewer itself, and a menu accelerator would fire the same action twice.
fn build_menu() -> (Menu, Submenu, Submenu) {
	let app_menu = Submenu::with_items(
		"Emulsion",
		true,
		&[
			&PredefinedMenuItem::about(None, None),
			&PredefinedMenuItem::separator(),
			&PredefinedMenuItem::services(None),
			&PredefinedMenuItem::separator(),
			&PredefinedMenuItem::hide(None),
			&PredefinedMenuItem::hide_others(None),
			&PredefinedMenuItem::show_all(None),
			&PredefinedMenuItem::separator(),
			&PredefinedMenuItem::quit(None),
		],
	)
	.expect("valid application menu");

	let file_menu = Submenu::with_items(
		"File",
		true,
		&[
			&item("open", "Open…", Some("Cmd+O")),
			&PredefinedMenuItem::separator(),
			&item("img_del", "Move to Trash", None),
			&PredefinedMenuItem::close_window(None),
		],
	)
	.expect("valid file menu");

	let edit_menu = Submenu::with_items("Edit", true, &[&item("img_copy", "Copy Image", None)])
		.expect("valid edit menu");

	let view_menu = Submenu::with_items(
		"View",
		true,
		&[
			&item("img_prev", "Previous Image", None),
			&item("img_next", "Next Image", None),
			&PredefinedMenuItem::separator(),
			&item("toggle_thumbnails", "Cycle Thumbnails", None),
			&PredefinedMenuItem::separator(),
			&item("img_orig", "Actual Size", None),
			&item("img_fit_best", "Best Fit", None),
			&item("img_fit", "Fit to Window", None),
			&PredefinedMenuItem::separator(),
			&item("gamma_decrease", "Decrease Gamma", None),
			&item("gamma_increase", "Increase Gamma", None),
			&item("gamma_reset", "Reset Gamma", None),
			&PredefinedMenuItem::separator(),
			&item("toggle_antialias", "Toggle Antialiasing", None),
			&item("play_anim", "Play Animation", None),
			&item("play_present", "Start Presentation", None),
			&item("play_present_rnd", "Start Shuffled Presentation", None),
			&PredefinedMenuItem::separator(),
			&item("toggle_fullscreen", "Enter Full Screen", Some("Cmd+Ctrl+F")),
		],
	)
	.expect("valid view menu");

	let window_menu = Submenu::with_items(
		"Window",
		true,
		&[
			&PredefinedMenuItem::minimize(None),
			&PredefinedMenuItem::maximize(None),
			&PredefinedMenuItem::separator(),
			&PredefinedMenuItem::bring_all_to_front(None),
		],
	)
	.expect("valid window menu");

	let help_menu = Submenu::with_items(
		"Help",
		true,
		&[
			&item("show_help", "Emulsion Help", Some("Cmd+?")),
			&item("visit_site", "Emulsion Website", None),
		],
	)
	.expect("valid help menu");

	let menu = Menu::with_items(&[
		&app_menu,
		&file_menu,
		&edit_menu,
		&view_menu,
		&window_menu,
		&help_menu,
	])
	.expect("valid macOS menu bar");
	(menu, window_menu, help_menu)
}

fn item(id: &'static str, title: &'static str, accelerator: Option<&str>) -> MenuItem {
	MenuItem::with_id(id, title, true, accelerator.and_then(parse_accelerator))
}

fn parse_accelerator(value: &str) -> Option<Accelerator> {
	let normalized =
		value.replace("CmdCtrl", "CmdOrCtrl").replace("Logo", "Cmd").replace("Return", "Enter");
	match normalized.parse() {
		Ok(accelerator) => Some(accelerator),
		Err(error) => {
			log::warn!("Ignoring menu accelerator {value:?}: {error}");
			None
		}
	}
}
