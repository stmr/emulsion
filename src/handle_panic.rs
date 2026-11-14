use std::env;
use std::fs::OpenOptions;
use std::io;
use std::io::Write;
use std::iter;
use std::panic;
use std::string::String;

use std::backtrace::Backtrace;

use crate::PROJECT_DIRS;

pub fn handle_panic(info: &panic::PanicHookInfo) {
	let trace = Backtrace::force_capture();

	let mut msg = String::new();

	let payload = info.payload();
	let payload_string = payload
		.downcast_ref::<&str>()
		.copied()
		.or_else(|| payload.downcast_ref::<String>().map(|s| s.as_str()));

	msg.push('\n');
	if let Some(panic_message) = payload_string {
		msg.push_str(&format!("\n--\n{}\n--\n\n", panic_message));
	}
	if let Some(location) = info.location() {
		msg.push_str(&format!(
			"Location {}:{}:{}\n\n",
			location.file(),
			location.line(),
			location.column()
		));
	}
	msg.push_str(&format!("{trace}\n"));
	for ch in iter::repeat_n('=', 99) {
		msg.push(ch);
	}

	eprintln!("\nPanic happened{}", msg);
	if let Err(e) = write_to_file(&msg) {
		eprintln!("Could not write panic to file: {e}");
	}
}

/// Past this size the log is rotated to `panic.old.txt` so it cannot grow without bound.
const MAX_PANIC_LOG_BYTES: u64 = 1024 * 1024;

fn write_to_file(msg: &str) -> io::Result<()> {
	let local_data_folder = if let Some(project_dirs) = &*PROJECT_DIRS {
		project_dirs.data_local_dir().to_owned()
	} else {
		let curr_exe = env::current_exe()?;
		curr_exe
			.parent()
			.ok_or_else(|| io::Error::other("Could not get exe parent folder!"))?
			.to_owned()
	};
	std::fs::create_dir_all(&local_data_folder)?;
	let path = local_data_folder.join("panic.txt");
	if std::fs::metadata(&path).is_ok_and(|m| m.len() > MAX_PANIC_LOG_BYTES) {
		std::fs::rename(&path, local_data_folder.join("panic.old.txt"))?;
	}
	let mut file = OpenOptions::new().create(true).append(true).open(path)?;

	write!(file, "{}", msg)?;
	Ok(())
}
