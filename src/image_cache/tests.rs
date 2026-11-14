//! Navigation, folder switching and rescans of `Directory`. They work on real (empty) files in a
//! temporary folder; the folder listing runs on its own thread, so they wait for it with a
//! deadline.

use std::{
	ffi::OsStr,
	fs,
	path::{Path, PathBuf},
	sync::atomic::{AtomicU32, Ordering},
	time::{Duration, Instant},
};

use super::directory::{Directory, ListingChange};

/// A unique folder that is removed on drop, also when the test fails.
struct TempDir(PathBuf);

impl TempDir {
	fn new(files: &[&str]) -> Self {
		static COUNTER: AtomicU32 = AtomicU32::new(0);
		let path = std::env::temp_dir().join(format!(
			"emulsion-nav-test-{}-{}",
			std::process::id(),
			COUNTER.fetch_add(1, Ordering::Relaxed)
		));
		fs::create_dir_all(&path).unwrap();
		let dir = TempDir(path);
		for name in files {
			dir.add(name);
		}
		dir
	}

	fn add(&self, name: &str) {
		fs::write(self.0.join(name), b"").unwrap();
	}

	fn remove(&self, name: &str) {
		fs::remove_file(self.0.join(name)).unwrap();
	}

	fn path(&self) -> &Path {
		&self.0
	}
}

impl Drop for TempDir {
	fn drop(&mut self) {
		let _ = fs::remove_dir_all(&self.0);
	}
}

fn wait_for_listing(dir: &mut Directory) -> ListingChange {
	let deadline = Instant::now() + Duration::from_secs(10);
	while Instant::now() < deadline {
		if let Some(change) = dir.poll_listing() {
			return change;
		}
		std::thread::sleep(Duration::from_millis(2));
	}
	panic!("the folder listing never arrived");
}

/// Shows `folder` with `filename` as the current file.
fn open(dir: &mut Directory, folder: &TempDir, filename: &str) {
	if !dir.change_directory_with_filename(folder.path(), OsStr::new(filename)).unwrap() {
		wait_for_listing(dir);
		assert!(dir.change_directory_with_filename(folder.path(), OsStr::new(filename)).unwrap());
	}
}

/// Lists the shown folder again and waits until the result is applied.
fn rescan(dir: &mut Directory) {
	dir.update_directory();
	assert_eq!(wait_for_listing(dir), ListingChange::Rescan);
}

fn current(dir: &Directory) -> String {
	dir.curr_filename().unwrap().to_string_lossy().into_owned()
}

fn request_id_of(dir: &Directory, name: &str) -> u32 {
	(0..dir.image_count().unwrap())
		.filter_map(|i| dir.image_by_index(i))
		.find(|item| item.path.file_name() == Some(OsStr::new(name)))
		.unwrap_or_else(|| panic!("{name} is not listed"))
		.request_id
}

#[test]
fn stepping_wraps_around_and_skips_non_images() {
	let folder = TempDir::new(&["a.png", "b.txt", "c.png", "d.jpg", "e.md"]);
	let mut dir = Directory::new();
	open(&mut dir, &folder, "c.png");
	assert_eq!(dir.image_count(), Some(3));
	assert_eq!(dir.curr_img_index(), Some(1));

	let mut seen = Vec::new();
	for _ in 0..4 {
		dir.jump_to_next();
		seen.push(current(&dir));
	}
	assert_eq!(seen, ["d.jpg", "a.png", "c.png", "d.jpg"]);

	let mut seen = Vec::new();
	for _ in 0..4 {
		dir.jump_to_prev();
		seen.push(current(&dir));
	}
	assert_eq!(seen, ["c.png", "a.png", "d.jpg", "c.png"]);
	assert_eq!(dir.curr_img_index(), Some(1));
}

#[test]
fn a_folder_without_images_does_not_navigate() {
	let folder = TempDir::new(&["notes.txt"]);
	let mut dir = Directory::new();
	assert!(!dir.change_directory(folder.path()).unwrap());
	wait_for_listing(&mut dir);
	assert_eq!(dir.image_count(), Some(0));
	dir.jump_to_next();
	dir.jump_to_prev();
	assert_eq!(dir.curr_img_index(), Some(0));
	assert!(dir.image_by_index(0).is_none());
	assert!(dir.set_curr_img_index(0).is_err());
}

#[test]
fn a_rescan_keeps_the_current_file_and_the_request_ids() {
	let folder = TempDir::new(&["a.png", "c.png", "d.png"]);
	let mut dir = Directory::new();
	open(&mut dir, &folder, "c.png");
	let (a_id, c_id, d_id) =
		(request_id_of(&dir, "a.png"), request_id_of(&dir, "c.png"), request_id_of(&dir, "d.png"));
	assert_eq!(dir.curr_descriptor().unwrap().request_id, c_id);

	// Files before the current one shift its index but not its identity.
	folder.add("0.png");
	folder.add("b.png");
	rescan(&mut dir);
	assert_eq!(current(&dir), "c.png");
	assert_eq!(dir.curr_img_index(), Some(3));
	assert_eq!(dir.image_count(), Some(5));
	assert_eq!(request_id_of(&dir, "a.png"), a_id);
	assert_eq!(request_id_of(&dir, "c.png"), c_id);
	assert_eq!(request_id_of(&dir, "d.png"), d_id);
	let new_ids = [request_id_of(&dir, "0.png"), request_id_of(&dir, "b.png")];
	assert!(new_ids.iter().all(|id| ![a_id, c_id, d_id].contains(id)));
	assert_ne!(new_ids[0], new_ids[1]);

	// Removing files before it does too, and the ids of the gone files stop being valid.
	folder.remove("a.png");
	folder.remove("0.png");
	rescan(&mut dir);
	assert_eq!(current(&dir), "c.png");
	assert_eq!(dir.curr_img_index(), Some(1));
	assert!(!dir.has_request(a_id));
	assert!(dir.has_request(c_id));
	assert_eq!(dir.img_index_of_request(c_id), Some(1));
	assert_eq!(dir.img_index_of_request(a_id), None);

	// A file that comes back is a new file: its old id is never handed out again.
	folder.add("a.png");
	rescan(&mut dir);
	assert_ne!(request_id_of(&dir, "a.png"), a_id);
	assert_eq!(request_id_of(&dir, "c.png"), c_id);
}

#[test]
fn a_non_image_has_a_request_id_but_no_image_index() {
	let folder = TempDir::new(&["a.png", "b.txt"]);
	let mut dir = Directory::new();
	open(&mut dir, &folder, "a.png");
	// Files are listed in display order, so ids are assigned a, b: the text file has the next id.
	let text_id = request_id_of(&dir, "a.png") + 1;
	assert!(dir.has_request(text_id));
	assert_eq!(dir.img_index_of_request(text_id), None);
}

#[test]
fn deleting_the_current_file_advances_to_the_next_image() {
	let folder = TempDir::new(&["a.png", "b.png", "c.png", "d.png"]);
	let mut dir = Directory::new();
	open(&mut dir, &folder, "b.png");

	folder.remove("b.png");
	rescan(&mut dir);
	assert_eq!(current(&dir), "c.png");
	assert_eq!(dir.curr_img_index(), Some(1));

	// The last image's successor is the first one.
	dir.jump_to_next();
	assert_eq!(current(&dir), "d.png");
	folder.remove("d.png");
	rescan(&mut dir);
	assert_eq!(current(&dir), "a.png");
	assert_eq!(dir.curr_img_index(), Some(0));

	// The next image skips whatever is not an image.
	folder.add("b.txt");
	rescan(&mut dir);
	folder.remove("a.png");
	rescan(&mut dir);
	assert_eq!(current(&dir), "c.png");

	folder.remove("c.png");
	rescan(&mut dir);
	assert_eq!(dir.image_count(), Some(0));
}

#[test]
fn switching_folders_keeps_the_old_listing_until_the_new_one_arrives() {
	let first = TempDir::new(&["a1.png", "a2.png"]);
	let second = TempDir::new(&["b1.png", "b2.png", "b3.png"]);
	let mut dir = Directory::new();
	open(&mut dir, &first, "a2.png");
	let old_ids = [request_id_of(&dir, "a1.png"), request_id_of(&dir, "a2.png")];

	assert!(!dir.change_directory(second.path()).unwrap());
	assert!(dir.listing_pending());
	assert_eq!(dir.path(), first.path());
	assert_eq!(dir.image_count(), Some(2));
	assert_eq!(current(&dir), "a2.png");

	assert_eq!(wait_for_listing(&mut dir), ListingChange::NewDirectory);
	assert_eq!(dir.path(), second.path());
	assert_eq!(dir.image_count(), Some(3));
	assert_eq!(current(&dir), "b1.png");
	assert_eq!(dir.curr_img_index(), Some(0));
	assert!(old_ids.iter().all(|&id| !dir.has_request(id)));
	let second_ids: Vec<u32> = (0..3).map(|i| dir.image_by_index(i).unwrap().request_id).collect();
	assert!(second_ids.iter().all(|id| !old_ids.contains(id)));

	// Coming back lists the folder again and never reuses an id of the other folders.
	assert!(!dir.change_directory(first.path()).unwrap());
	assert_eq!(wait_for_listing(&mut dir), ListingChange::NewDirectory);
	assert_eq!(dir.image_count(), Some(2));
	let back_ids = [request_id_of(&dir, "a1.png"), request_id_of(&dir, "a2.png")];
	assert!(back_ids.iter().all(|id| !old_ids.contains(id) && !second_ids.contains(id)));
}

#[test]
fn opening_a_file_in_another_folder_selects_it_once_listed() {
	let first = TempDir::new(&["a.png"]);
	let second = TempDir::new(&["b1.png", "b2.png"]);
	let mut dir = Directory::new();
	open(&mut dir, &first, "a.png");

	assert!(!dir.change_directory_with_filename(second.path(), OsStr::new("b2.png")).unwrap());
	wait_for_listing(&mut dir);
	assert!(dir.change_directory_with_filename(second.path(), OsStr::new("b2.png")).unwrap());
	assert_eq!(current(&dir), "b2.png");
	assert_eq!(dir.curr_img_index(), Some(1));
	assert!(dir.change_directory_with_filename(second.path(), OsStr::new("missing.png")).is_err());
}

#[test]
fn only_the_last_requested_folder_is_applied() {
	let first = TempDir::new(&["a.png"]);
	let second = TempDir::new(&["b1.png", "b2.png"]);
	let third = TempDir::new(&["c1.png", "c2.png", "c3.png"]);
	let mut dir = Directory::new();
	open(&mut dir, &first, "a.png");

	// The second listing is superseded before it can be applied.
	assert!(!dir.change_directory(second.path()).unwrap());
	assert!(!dir.change_directory(third.path()).unwrap());
	assert_eq!(wait_for_listing(&mut dir), ListingChange::NewDirectory);
	assert_eq!(dir.path(), third.path());
	assert_eq!(dir.image_count(), Some(3));
	std::thread::sleep(Duration::from_millis(30));
	assert_eq!(dir.poll_listing(), None);
	assert_eq!(dir.path(), third.path());
	assert!(!dir.listing_pending());
}

#[test]
fn asking_for_the_shown_folder_cancels_a_pending_switch() {
	let first = TempDir::new(&["a.png"]);
	let second = TempDir::new(&["b1.png", "b2.png"]);
	let mut dir = Directory::new();
	open(&mut dir, &first, "a.png");

	assert!(!dir.change_directory(second.path()).unwrap());
	assert!(dir.change_directory(first.path()).unwrap());
	assert!(!dir.listing_pending());
	// Whatever the lister still produces for the other folder is ignored.
	std::thread::sleep(Duration::from_millis(30));
	assert_eq!(dir.poll_listing(), None);
	assert_eq!(dir.path(), first.path());
	assert_eq!(dir.image_count(), Some(1));
}

#[test]
fn prefetch_order_alternates_sides_and_wraps() {
	assert_eq!(super::prefetch_order(0, 0).count(), 0);
	assert_eq!(super::prefetch_order(0, 1).count(), 0);
	assert_eq!(super::prefetch_order(0, 2).collect::<Vec<_>>(), [1]);
	assert_eq!(super::prefetch_order(1, 2).collect::<Vec<_>>(), [0]);
	assert_eq!(super::prefetch_order(2, 5).collect::<Vec<_>>(), [3, 1, 4, 0]);
	assert_eq!(super::prefetch_order(0, 5).collect::<Vec<_>>(), [1, 4, 2, 3]);
	assert_eq!(super::prefetch_order(4, 5).collect::<Vec<_>>(), [0, 3, 1, 2]);
	assert_eq!(super::prefetch_order(0, 4).collect::<Vec<_>>(), [1, 3, 2]);
}

#[test]
fn prefetch_order_visits_every_other_index_once() {
	for count in 0..12 {
		for curr in 0..count {
			let mut order = super::prefetch_order(curr, count).collect::<Vec<_>>();
			order.sort_unstable();
			let want = (0..count).filter(|&i| i != curr).collect::<Vec<_>>();
			assert_eq!(order, want, "curr {curr} of {count}");
		}
	}
}
