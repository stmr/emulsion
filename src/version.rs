// `Version` only serves the update check, which needs the `networking` feature.
#[cfg(feature = "networking")]
use std::{fmt, num::ParseIntError, str::FromStr};

/// A semver version
#[cfg(feature = "networking")]
#[derive(Debug, Default, PartialEq, PartialOrd, Eq, Ord)]
pub struct Version {
	major: u32,
	minor: u32,
	patch: u32,
}

#[cfg(feature = "networking")]
impl Version {
	/// Return the version of the cargo build
	pub fn cargo_pkg_version() -> Self {
		let major = env!("CARGO_PKG_VERSION_MAJOR").parse().expect("Invalid cargo version");
		let minor = env!("CARGO_PKG_VERSION_MINOR").parse().expect("Invalid cargo version");
		let patch = env!("CARGO_PKG_VERSION_PATCH").parse().expect("Invalid cargo version");

		Version { major, minor, patch }
	}
}

/// Commit the binary was built from (`-dirty` with local changes); empty outside a git checkout.
pub const BUILD_ID: &str = env!("EMULSION_BUILD");

/// `13.0.0 (1a2b3c4d5)`, or just the version when the build id is unknown.
pub fn display_version() -> String {
	match BUILD_ID {
		"" => env!("CARGO_PKG_VERSION").to_owned(),
		id => format!("{} ({id})", env!("CARGO_PKG_VERSION")),
	}
}

#[cfg(feature = "networking")]
impl fmt::Display for Version {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
	}
}

#[cfg(feature = "networking")]
impl FromStr for Version {
	type Err = ParseIntError;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		// Trim letters from the start of the version tag, e.g. "v1.9" -> "1.9"
		let mut iter = s.trim_start_matches(char::is_alphabetic).split('.');
		// Consume the next part of the version tag
		let mut extract_part = || iter.next().filter(|&n| !n.is_empty()).unwrap_or("0");

		let major = extract_part().parse()?;
		let minor = extract_part().parse()?;
		let patch = extract_part().parse()?;

		Ok(Version { major, minor, patch })
	}
}
