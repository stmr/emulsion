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
	git_build_id();
	platform_specific();
}

/// Sets `EMULSION_BUILD` to the short commit hash, with `-dirty` when tracked files differ from
/// it, so `--version` and the About panel identify the exact build. Empty outside a git checkout
/// (e.g. a crates.io source build).
fn git_build_id() {
	let git = |args: &[&str]| {
		std::process::Command::new("git")
			.args(args)
			.output()
			.ok()
			.filter(|out| out.status.success())
			.map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
	};
	let id = match git(&["rev-parse", "--short=9", "HEAD"]) {
		Some(hash) => {
			// `--no-optional-locks` keeps `status` from rewriting the index.
			let status =
				git(&["--no-optional-locks", "status", "--porcelain", "--untracked-files=no"]);
			if status.is_some_and(|s| !s.is_empty()) { format!("{hash}-dirty") } else { hash }
		}
		None => String::new(),
	};
	println!("cargo:rustc-env=EMULSION_BUILD={id}");
	// New commits move HEAD or the branch ref; edits to tracked sources change the dirty flag.
	for path in ["HEAD", "packed-refs"].into_iter().map(str::to_owned).chain(git(&[
		"symbolic-ref",
		"-q",
		"HEAD",
	])) {
		if let Some(file) = git(&["rev-parse", "--git-path", &path]) {
			println!("cargo:rerun-if-changed={file}");
		}
	}
	for path in ["src", "subcrates", "resource", "Cargo.toml", "Cargo.lock"] {
		println!("cargo:rerun-if-changed={path}");
	}
}
