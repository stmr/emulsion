use std::env;

fn main() {
	println!("cargo:rerun-if-changed=build.rs");
	println!("cargo:rustc-check-cfg=cfg(free_unix)");

	// Platforms where winit provides the X11/Wayland backends (same definition winit uses).
	let os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
	let free_unix = env::var_os("CARGO_CFG_UNIX").is_some()
		&& !matches!(os.as_str(), "macos" | "ios" | "android" | "emscripten" | "redox");
	if free_unix {
		println!("cargo:rustc-cfg=free_unix");
	}
}
