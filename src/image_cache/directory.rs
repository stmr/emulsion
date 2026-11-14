use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use super::image_loader::is_file_supported;
use crate::parallel_action::ParallelAction;

#[derive(Debug)]
pub enum Error {
	Other(String),
}
impl fmt::Display for Error {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self {
			Error::Other(s) => f.write_fmt(format_args!("Other error: {}", s)),
		}
	}
}
impl std::error::Error for Error {}
impl From<std::io::Error> for Error {
	fn from(e: std::io::Error) -> Self {
		Error::Other(format!("{}", e))
	}
}
impl From<Error> for std::io::Error {
	fn from(value: Error) -> Self {
		std::io::Error::other(format!("directory::Error: {}", value))
	}
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Clone)]
pub struct DirItem {
	pub path: PathBuf,

	/// Sometimes also abbreviated as `req_id` is used as a more efficient replacement
	/// of a PathBuf to identify a file load request.
	pub request_id: u32,
}

/// What `Directory::poll_listing` applied.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ListingChange {
	/// The folder that was already shown was listed again.
	Rescan,
	/// Another folder replaced the shown one: none of the previous files are listed.
	NewDirectory,
}

/// What the lister thread read from a folder: the files in display order and the positions in
/// that list of those that are images.
struct Listing {
	path: PathBuf,
	result: std::io::Result<(Vec<PathBuf>, Vec<usize>)>,
}

pub struct Directory {
	/// The folder `files` was listed from. Empty until the first listing arrives.
	path: PathBuf,
	files: Vec<DirItem>,

	/// Maps image indicies to indicies for the `files` vector.
	/// For example one could use it like `files[image_indicies[i]]`
	img_i_to_file_i: Vec<usize>,

	/// Maps file indicies to indicies for the `curr_image_idx`.
	/// This is relevant when the current image is given by its name
	/// when it will first be located by its file index.
	file_i_to_img_i: Vec<Option<u32>>,

	/// A monotonically increasing integer used for identifying
	/// each load request
	current_req_id: u32,

	/// Maps request ids to indicies for the `files` vector.
	file_idx_by_req: HashMap<u32, usize>,

	/// current file index
	/// This must never be exposed to users of this object.
	curr_file_idx: usize,

	/// Current image index.
	/// Use this value to index the `image_indicies` vector to find the apppropriate file index.
	curr_image_idx: usize,

	/// The folder `lister` is working on. Until its listing arrives, `files` keeps describing
	/// the previous folder (or the same folder before the rescan), so navigation carries on.
	pending: Option<PathBuf>,

	/// A folder that could not be listed. Reported once to whoever asks for it again.
	failed: Option<(PathBuf, Error)>,

	/// Reading a folder and testing every file in it can take long, so it runs off the UI thread.
	lister: ParallelAction<PathBuf, Listing>,
}

fn file_name_lossy(path: &Path) -> Cow<'_, str> {
	path.file_name().unwrap_or_default().to_string_lossy()
}

/// Display order of a folder. Names that are equal for the natural order (`IMG.jpg` and
/// `img.jpg`) fall back to the whole path, so the order doesn't depend on how the OS listed them.
fn compare_files(a: &Path, b: &Path) -> Ordering {
	lexical_sort::natural_lexical_cmp(&file_name_lossy(a), &file_name_lossy(b))
		.then_with(|| a.cmp(b))
}

fn read_sorted_files(dir: &Path) -> std::io::Result<Vec<PathBuf>> {
	let mut files: Vec<PathBuf> = fs::read_dir(dir)?
		.filter_map(|entry| {
			let entry = entry.ok()?;
			let file_type = entry.file_type().ok()?;
			(file_type.is_file() || file_type.is_symlink()).then(|| entry.path())
		})
		.collect();
	files.sort_unstable_by(|a, b| compare_files(a, b));
	Ok(files)
}

fn list_directory(path: PathBuf) -> Listing {
	let result = read_sorted_files(&path).map(|files| {
		let images = files
			.iter()
			.enumerate()
			.filter(|(_, file)| is_file_supported(file))
			.map(|(i, _)| i)
			.collect();
		(files, images)
	});
	Listing { path, result }
}

/// The image following the file at `file_idx` (which may not be an image itself), wrapping
/// around. `img_i_to_file_i` is ascending.
fn next_image(img_i_to_file_i: &[usize], file_idx: usize) -> Option<usize> {
	let after = img_i_to_file_i.partition_point(|&f| f <= file_idx);
	(!img_i_to_file_i.is_empty()).then(|| after % img_i_to_file_i.len())
}

/// The image preceding the file at `file_idx`, wrapping around.
fn prev_image(img_i_to_file_i: &[usize], file_idx: usize) -> Option<usize> {
	let before = img_i_to_file_i.partition_point(|&f| f < file_idx);
	let count = img_i_to_file_i.len();
	(count > 0).then(|| before.checked_sub(1).unwrap_or(count - 1))
}

/// File index of the first image at or after `file_idx`; the first image if there is none
/// and 0 if the folder has no images.
fn image_file_at_or_after(img_i_to_file_i: &[usize], file_idx: usize) -> usize {
	let after = img_i_to_file_i.partition_point(|&f| f < file_idx);
	img_i_to_file_i.get(after).or(img_i_to_file_i.first()).copied().unwrap_or(0)
}

impl Directory {
	pub fn new() -> Self {
		Directory {
			path: PathBuf::new(),
			files: Vec::new(),
			img_i_to_file_i: Vec::new(),
			file_i_to_img_i: Vec::new(),
			curr_file_idx: 0,
			curr_image_idx: 0,
			current_req_id: 0,
			file_idx_by_req: HashMap::new(),
			pending: None,
			failed: None,
			lister: ParallelAction::new(list_directory),
		}
	}

	fn request_listing(&mut self, path: &Path) {
		self.failed = None;
		self.pending = Some(path.to_owned());
		self.lister.give_input(path.to_owned());
	}

	/// Makes `path` the folder to show. Returns `true` if it is the shown folder already.
	/// Otherwise it starts listing it in the background and returns `false`; the previous
	/// listing stays in use until `poll_listing` brings the new one. Asking for the shown
	/// folder again cancels a pending switch to another one.
	pub fn change_directory(&mut self, path: &Path) -> Result<bool> {
		match self.failed.take() {
			Some((failed, error)) if failed == path => return Err(error),
			other => self.failed = other,
		}
		let listing_it = self.pending.as_deref() == Some(path);
		if self.path == path {
			if !listing_it {
				self.pending = None;
			}
			Ok(true)
		} else {
			if !listing_it {
				self.request_listing(path);
			}
			Ok(false)
		}
	}

	/// Like `change_directory`, and when the folder is shown, makes the file the current one.
	pub fn change_directory_with_filename(
		&mut self,
		path: &Path,
		filename: &OsStr,
	) -> Result<bool> {
		if !self.change_directory(path)? {
			return Ok(false);
		}
		let Some(index) =
			self.files.iter().position(|desc| desc.path.file_name() == Some(filename))
		else {
			return Err(Error::Other(format!(
				"Could not find file {:?} in directory {:?}",
				filename, path
			)));
		};
		if self.file_i_to_img_i.get(index).copied().flatten().is_none() {
			return Err(Error::Other(format!(
				"{:?} is not a supported image file",
				self.files[index].path
			)));
		}
		self.curr_file_idx = index;
		self.set_image_index_from_file_index();
		Ok(true)
	}

	/// Applies the listing the background thread has finished, if there is one.
	pub fn poll_listing(&mut self) -> Option<ListingChange> {
		let Listing { path, result } = self.lister.try_get_output()?;
		// The listing of a folder nobody wants any more.
		if self.pending.as_deref() != Some(path.as_path()) {
			return None;
		}
		self.pending = None;
		match result {
			Ok((files, image_indices)) => Some(self.apply_listing(path, files, image_indices)),
			Err(error) => {
				log::error!("Could not list the files of {:?}: {}", path, error);
				if path != self.path {
					self.failed = Some((path, error.into()));
				}
				None
			}
		}
	}

	fn apply_listing(
		&mut self,
		path: PathBuf,
		files: Vec<PathBuf>,
		image_indices: Vec<usize>,
	) -> ListingChange {
		let rescan = self.path == path;
		let curr_filename = if rescan { self.curr_filename() } else { None };
		let prev_file_idx = self.curr_file_idx;

		// Files that were already listed keep their request id so that their cached textures
		// stay valid across a refresh of the directory.
		let known_ids: HashMap<PathBuf, u32> =
			std::mem::take(&mut self.files).into_iter().map(|d| (d.path, d.request_id)).collect();
		self.files = files
			.into_iter()
			.map(|path| {
				let request_id = known_ids.get(&path).copied().unwrap_or_else(|| {
					self.current_req_id += 1;
					self.current_req_id
				});
				DirItem { path, request_id }
			})
			.collect();
		self.file_idx_by_req =
			self.files.iter().enumerate().map(|(i, item)| (item.request_id, i)).collect();
		self.img_i_to_file_i = image_indices;
		self.path = path;

		// Stay on the current file; if it is gone, move to the image that took its place.
		self.curr_file_idx = curr_filename
			.and_then(|name| self.files.iter().position(|d| d.path.file_name() == Some(&name)))
			.unwrap_or_else(|| {
				image_file_at_or_after(
					&self.img_i_to_file_i,
					if rescan { prev_file_idx } else { 0 },
				)
			});
		self.curr_image_idx = 0;
		self.finished_filtering();
		if rescan { ListingChange::Rescan } else { ListingChange::NewDirectory }
	}

	/// Lists the shown folder again, unless a listing is under way already.
	pub fn update_directory(&mut self) {
		if self.pending.is_none() && !self.path.as_os_str().is_empty() {
			let path = self.path.clone();
			self.request_listing(&path);
		}
	}

	/// Whether a folder listing is under way.
	pub fn listing_pending(&self) -> bool {
		self.pending.is_some()
	}

	/// No folder has been listed yet but one is on its way.
	fn awaiting_first_listing(&self) -> bool {
		self.path.as_os_str().is_empty() && self.pending.is_some()
	}

	pub fn curr_filename(&self) -> Option<OsString> {
		self.files.get(self.curr_file_idx).and_then(|x| x.path.file_name().map(|x| x.to_owned()))
	}

	pub fn curr_descriptor(&self) -> Option<&DirItem> {
		self.files.get(self.curr_file_idx)
	}

	pub fn path(&self) -> &Path {
		self.path.as_path()
	}

	pub fn set_curr_img_index(&mut self, index: usize) -> Result<()> {
		if let Some(file_idx) = self.img_i_to_file_i.get(index)
			&& *file_idx < self.files.len()
		{
			self.curr_file_idx = *file_idx;
			self.curr_image_idx = index;
			return Ok(());
		}
		Err(Error::Other("Could not find image index".to_string()))
	}

	/// Maps a request id of a listed file to its image index. `None` if the file is gone or is
	/// not an image.
	pub fn img_index_of_request(&self, req_id: u32) -> Option<usize> {
		let file_idx = *self.file_idx_by_req.get(&req_id)?;
		self.file_i_to_img_i.get(file_idx).copied().flatten().map(|i| i as usize)
	}

	/// Whether `req_id` identifies a file of the current listing.
	pub fn has_request(&self, req_id: u32) -> bool {
		self.file_idx_by_req.contains_key(&req_id)
	}

	fn set_curr_image(&mut self, img_idx: Option<usize>) {
		if let Some(img_idx) = img_idx {
			self.curr_image_idx = img_idx;
			self.curr_file_idx = self.img_i_to_file_i[img_idx];
		}
	}

	pub fn jump_to_prev(&mut self) {
		self.set_curr_image(prev_image(&self.img_i_to_file_i, self.curr_file_idx));
	}

	pub fn jump_to_next(&mut self) {
		self.set_curr_image(next_image(&self.img_i_to_file_i, self.curr_file_idx));
	}

	/// Returns none while the very first folder is being listed
	pub fn curr_img_index(&self) -> Option<usize> {
		(!self.awaiting_first_listing()).then_some(self.curr_image_idx)
	}

	/// The number of images in the shown folder, or none while the very first folder is being
	/// listed.
	pub fn image_count(&self) -> Option<usize> {
		(!self.awaiting_first_listing()).then_some(self.img_i_to_file_i.len())
	}

	/// Return None if there is no such image in the shown folder
	pub fn image_by_index(&self, idx: usize) -> Option<&DirItem> {
		self.img_i_to_file_i.get(idx).map(|&i| &self.files[i])
	}

	fn finished_filtering(&mut self) {
		self.file_i_to_img_i.clear();
		self.file_i_to_img_i.reserve(self.files.len());
		let mut last_file_i: isize = -1;
		for (curr_img_i, &curr_file_i) in self.img_i_to_file_i.iter().enumerate() {
			for _ in (last_file_i + 1) as usize..curr_file_i {
				self.file_i_to_img_i.push(None);
			}
			self.file_i_to_img_i.push(Some(curr_img_i as u32));
			last_file_i = curr_file_i as isize;
		}
		self.set_image_index_from_file_index();
	}

	fn set_image_index_from_file_index(&mut self) {
		// The current file is `None` here when it is not an image (or lies past the last image).
		if let Some(Some(img_idx)) = self.file_i_to_img_i.get(self.curr_file_idx) {
			self.curr_image_idx = *img_idx as usize;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::{Duration, Instant};

	/// `natural_lexical_cmp` only ties names that are the same once converted to a string, which
	/// happens with names that differ in bytes that aren't valid UTF-8. The listing order of
	/// the OS must not decide which of those comes first.
	#[cfg(unix)]
	#[test]
	fn names_equal_for_natural_order_sort_by_path() {
		use std::os::unix::ffi::OsStringExt;
		let path = |name: &[u8]| PathBuf::from(OsString::from_vec(name.to_vec()));
		let (a, b) = (path(b"img\xfe.jpg"), path(b"img\xff.jpg"));
		assert_eq!(
			lexical_sort::natural_lexical_cmp(&file_name_lossy(&a), &file_name_lossy(&b)),
			Ordering::Equal
		);
		let sorted = |mut files: Vec<PathBuf>| {
			files.sort_unstable_by(|a, b| compare_files(a, b));
			files
		};
		let forward = sorted(vec![a.clone(), b.clone()]);
		assert_eq!(forward, [a.clone(), b.clone()]);
		assert_eq!(sorted(vec![b, a]), forward);
	}

	#[test]
	fn image_stepping_wraps_and_skips_non_images() {
		// Files 0..=5, of which 1, 3 and 4 are images.
		let images = [1, 3, 4];
		assert_eq!(next_image(&images, 1), Some(1));
		assert_eq!(next_image(&images, 4), Some(0));
		assert_eq!(next_image(&images, 0), Some(0));
		assert_eq!(next_image(&images, 5), Some(0));
		assert_eq!(prev_image(&images, 3), Some(0));
		assert_eq!(prev_image(&images, 1), Some(2));
		assert_eq!(prev_image(&images, 2), Some(0));
		assert_eq!(prev_image(&images, 0), Some(2));
		assert_eq!(next_image(&[], 0), None);
		assert_eq!(prev_image(&[], 0), None);

		assert_eq!(image_file_at_or_after(&images, 2), 3);
		assert_eq!(image_file_at_or_after(&images, 4), 4);
		assert_eq!(image_file_at_or_after(&images, 5), 1);
		assert_eq!(image_file_at_or_after(&[], 5), 0);
	}

	fn wait_for_listing(dir: &mut Directory) -> ListingChange {
		let deadline = Instant::now() + Duration::from_secs(10);
		while Instant::now() < deadline {
			if let Some(change) = dir.poll_listing() {
				return change;
			}
			std::thread::sleep(Duration::from_millis(5));
		}
		panic!("the folder listing never arrived");
	}

	/// The listing comes from another thread; the shown folder keeps working until it lands, and a
	/// rescan keeps the current file.
	#[test]
	fn listing_is_applied_when_it_arrives() {
		let folder = std::env::temp_dir().join(format!("emulsion-dir-test-{}", std::process::id()));
		fs::create_dir_all(&folder).unwrap();
		for name in ["b.png", "a.png", "notes.txt"] {
			fs::write(folder.join(name), b"").unwrap();
		}

		let mut dir = Directory::new();
		assert!(!dir.change_directory_with_filename(&folder, OsStr::new("b.png")).unwrap());
		assert_eq!(dir.image_count(), None);
		assert_eq!(wait_for_listing(&mut dir), ListingChange::NewDirectory);
		assert_eq!(dir.image_count(), Some(2));
		assert!(dir.change_directory_with_filename(&folder, OsStr::new("b.png")).unwrap());
		assert_eq!(dir.curr_filename().as_deref(), Some(OsStr::new("b.png")));
		assert!(dir.change_directory_with_filename(&folder, OsStr::new("notes.txt")).is_err());

		// A new file shifts the indices; the current file stays current and keeps its request id.
		let req_id = dir.curr_descriptor().unwrap().request_id;
		fs::write(folder.join("0.png"), b"").unwrap();
		dir.update_directory();
		assert!(dir.listing_pending());
		assert_eq!(dir.image_count(), Some(2));
		assert_eq!(wait_for_listing(&mut dir), ListingChange::Rescan);
		assert_eq!(dir.image_count(), Some(3));
		assert_eq!(dir.curr_filename().as_deref(), Some(OsStr::new("b.png")));
		assert_eq!(dir.curr_descriptor().unwrap().request_id, req_id);

		// When the current file disappears the next image takes over.
		fs::remove_file(folder.join("b.png")).unwrap();
		dir.update_directory();
		assert_eq!(wait_for_listing(&mut dir), ListingChange::Rescan);
		assert_eq!(dir.image_count(), Some(2));
		assert!(!dir.has_request(req_id));
		assert_eq!(dir.curr_filename().as_deref(), Some(OsStr::new("0.png")));

		// A folder that can't be listed is reported to whoever asks for it.
		let missing = folder.join("missing");
		assert!(!dir.change_directory(&missing).unwrap());
		let deadline = Instant::now() + Duration::from_secs(10);
		while dir.listing_pending() && Instant::now() < deadline {
			dir.poll_listing();
			std::thread::sleep(Duration::from_millis(5));
		}
		assert!(dir.change_directory(&missing).is_err());
		assert!(dir.change_directory(&folder).unwrap());

		fs::remove_dir_all(&folder).unwrap();
	}
}
