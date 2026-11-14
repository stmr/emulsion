/// Total physical memory in bytes, or `None` if the platform does not report it.
#[cfg(target_os = "macos")]
pub fn total_memory_bytes() -> Option<u64> {
	let mut value: u64 = 0;
	let mut len = std::mem::size_of::<u64>();
	// SAFETY: the name is NUL-terminated and `value`/`len` describe a valid u64 buffer.
	let status = unsafe {
		libc::sysctlbyname(
			c"hw.memsize".as_ptr(),
			(&raw mut value).cast(),
			&mut len,
			std::ptr::null_mut(),
			0,
		)
	};
	(status == 0).then_some(value)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn total_memory_bytes() -> Option<u64> {
	// SAFETY: `sysinfo` only writes into the zero-initialised struct it is given.
	let info = unsafe {
		let mut info: libc::sysinfo = std::mem::zeroed();
		if libc::sysinfo(&mut info) != 0 {
			return None;
		}
		info
	};
	Some(info.totalram as u64 * u64::from(info.mem_unit.max(1)))
}

#[cfg(windows)]
pub fn total_memory_bytes() -> Option<u64> {
	use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
	// SAFETY: `dwLength` is set as the API requires and the struct is otherwise plain data.
	unsafe {
		let mut status: MEMORYSTATUSEX = std::mem::zeroed();
		status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
		(GlobalMemoryStatusEx(&mut status) != 0).then_some(status.ullTotalPhys)
	}
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "android", windows)))]
pub fn total_memory_bytes() -> Option<u64> {
	None
}

#[cfg(test)]
mod tests {
	#[test]
	fn reports_plausible_total() {
		let total = super::total_memory_bytes().expect("memory size unavailable");
		assert!(total >= 512 * 1024 * 1024, "{total}");
	}
}
