#[cfg(windows)]
extern crate winres;

#[cfg(windows)]
fn platform_specific() {
	let mut res = winres::WindowsResource::new();
	res.set("FileDescription", "Emulsion");
	res.set_icon("resource_dev/emulsion.ico");
	res.compile().unwrap();
	println!("cargo:rerun-if-changed=resource_dev/emulsion.ico");
}

#[cfg(unix)]
fn platform_specific() {}

fn main() {
	println!("cargo:rerun-if-changed=build.rs");
	platform_specific();
}
