use std::marker::PhantomData;
use std::mem;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use rand::rng;
use rand::seq::SliceRandom;

use log::{debug, error, trace};

use gelatin::Display;
use gelatin::window::Window;

use crate::image_cache::{
	self, AnimationFrameTexture, ImageCache, PathResolutionError, PathedTextureResult,
	TextureResult,
};

const NANOS_PER_SEC: u64 = 1_000_000_000;

#[derive(Debug, Eq, PartialEq)]
pub enum LoadRequest {
	None,
	LoadNext,
	LoadPrevious,
	FilePath(PathBuf),
	LoadAtIndex(usize),
	Jump(i32),
}

#[derive(Eq, PartialEq, Copy, Clone)]
pub enum PlaybackState {
	Paused,
	Forward,
	Present,
	RandomPresent,
}

trait Playback: Sized {
	fn load_next(image_cache: &mut ImageCache, display: &Display) -> PathedTextureResult;

	fn load_prev(image_cache: &mut ImageCache, display: &Display) -> PathedTextureResult;

	fn load_jump(
		image_cache: &mut ImageCache,
		display: &Display,
		amount: i32,
	) -> PathedTextureResult;

	fn load_path(
		image_cache: &mut ImageCache,
		display: &Display,
		path: &Path,
	) -> TextureResult<AnimationFrameTexture> {
		image_cache.load_specific(display, path, None)
	}

	fn load_at_index(
		image_cache: &mut ImageCache,
		display: &Display,
		index: usize,
	) -> PathedTextureResult;

	fn delay_nanos(player: &ImgSequencePlayer<Self>) -> u64;
}

struct FolderPlayback;

impl Playback for FolderPlayback {
	fn load_next(image_cache: &mut ImageCache, display: &Display) -> PathedTextureResult {
		image_cache.load_next(display)
	}

	fn load_prev(image_cache: &mut ImageCache, display: &Display) -> PathedTextureResult {
		image_cache.load_prev(display)
	}

	fn load_jump(
		image_cache: &mut ImageCache,
		display: &Display,
		amount: i32,
	) -> PathedTextureResult {
		image_cache.load_jump(display, amount, 0)
	}

	fn load_at_index(
		image_cache: &mut ImageCache,
		display: &Display,
		index: usize,
	) -> PathedTextureResult {
		image_cache.load_at_index(display, index, None)
	}

	fn delay_nanos(_player: &ImgSequencePlayer<Self>) -> u64 {
		const FRAMERATE: u64 = 25;
		NANOS_PER_SEC / FRAMERATE
	}
}

struct AnimPlayback;

impl Playback for AnimPlayback {
	fn load_next(image_cache: &mut ImageCache, display: &Display) -> PathedTextureResult {
		image_cache.load_jump(display, 0, 1)
	}

	fn load_prev(image_cache: &mut ImageCache, display: &Display) -> PathedTextureResult {
		image_cache.load_jump(display, 0, -1)
	}

	fn load_jump(
		image_cache: &mut ImageCache,
		display: &Display,
		amount: i32,
	) -> PathedTextureResult {
		image_cache.load_jump(display, 0, amount as isize)
	}

	fn load_at_index(
		image_cache: &mut ImageCache,
		display: &Display,
		index: usize,
	) -> PathedTextureResult {
		if let Some(curr_index) = image_cache.current_file_index() {
			image_cache.load_at_index(display, curr_index, Some(index as isize))
		} else {
			Err(PathResolutionError::WaitingOnDirFilter)
		}
	}

	fn delay_nanos(player: &ImgSequencePlayer<Self>) -> u64 {
		if let Some(ref frame) = player.image_texture { frame.delay_nano } else { 0 }
	}
}

pub struct PlaybackManager {
	image_cache: ImageCache,

	folder_player: ImgSequencePlayer<FolderPlayback>,
	image_player: ImgSequencePlayer<AnimPlayback>,

	/// A rescan of the folder was started and its result has not been acted on yet.
	rescanning: bool,
}

impl PlaybackManager {
	pub fn new() -> Self {
		let cache_capaxity = match crate::system_memory::total_memory_bytes() {
			Some(total) => (total / 8) as isize,
			None => {
				eprintln!("Could not get system memory size, using default value");
				// bytes
				500_000_000
			}
		};

		let thread_count =
			std::thread::available_parallelism().map_or(4, |n| n.get().clamp(2, 4) as u32);

		PlaybackManager {
			image_cache: ImageCache::new(cache_capaxity, thread_count),
			folder_player: ImgSequencePlayer::new(),
			image_player: ImgSequencePlayer::new(),
			rescanning: false,
		}
	}

	pub fn playback_state(&self) -> PlaybackState {
		self.folder_player.playback_state()
	}

	pub fn start_playback_forward(&mut self) {
		self.folder_player.start_playback_forward();
	}

	pub fn pause_playback(&mut self) {
		self.folder_player.pause_playback();
	}

	pub fn start_random_presentation(&mut self) {
		self.folder_player.start_random_presentation(&mut self.image_cache);
	}

	pub fn start_presentation(&mut self) {
		self.folder_player.start_presentation();
	}

	/// Returns None while the very first folder is being listed
	pub fn current_file_index(&mut self) -> Option<usize> {
		self.image_cache.current_file_index()
	}

	/// Returns None while the very first folder is being listed
	pub fn current_dir_len(&mut self) -> Option<usize> {
		self.image_cache.current_dir_len()
	}

	pub fn path_by_index(&mut self, index: usize) -> Option<std::path::PathBuf> {
		self.image_cache.path_by_index(index)
	}

	/// Starts listing the shown folder again in the background; `update_image` reloads the
	/// current file when the listing arrives.
	pub fn update_directory(&mut self) {
		debug!("In `update_directory`");
		if let LoadRequest::None = self.folder_player.load_request {
			let curr_path = self.image_cache.current_file_path();
			debug!("In `update_directory`, current_file_path is: {:?}", curr_path);
			if curr_path.is_some() {
				self.image_cache.update_directory();
				self.rescanning = true;
			}
		}
	}

	pub fn request_load(&mut self, request: LoadRequest) {
		// Navigation has nothing to act on until the first folder is listed; it must not
		// replace the request to open that folder.
		if self.image_cache.current_dir_len().is_none()
			&& !matches!(request, LoadRequest::FilePath(_))
		{
			return;
		}
		self.folder_player.request_load(request);
		self.image_player.request_load(LoadRequest::Jump(0));
	}

	pub fn image_texture(&self) -> Option<AnimationFrameTexture> {
		self.image_player.image_texture()
	}

	/// Tells the cache the area (physical pixels) SVGs are fitted to when they are loaded.
	pub fn set_svg_view(&mut self, width: u32, height: u32) {
		self.image_cache.set_svg_view(width, height);
	}

	/// Renders the shown SVG again to fit `fit` (physical pixels); `image_texture` keeps
	/// returning the old render until the new one is ready. `false`: not possible right now.
	pub fn rerender_svg(&mut self, fit: (u32, u32)) -> bool {
		let started = self.image_cache.rerender_svg(fit);
		if started {
			// Makes the players poll for the new render.
			self.request_load(LoadRequest::Jump(0));
		}
		started
	}

	/// The path to the image file which is currently rendered onto the screen.
	pub fn shown_file_path(&self) -> &LoadedImgPath {
		&self.folder_player.file_path
	}

	pub fn update_image(&mut self, window: &Window) -> gelatin::NextUpdate {
		let display = window.display_mut();
		if self.rescanning && !self.image_cache.listing_pending() {
			self.rescanning = false;
			if let LoadRequest::None = self.folder_player.load_request {
				// The folder changed: show the file that is current now. If there is none, the
				// empty path hides the previously loaded image.
				let path = self.image_cache.current_file_path().unwrap_or_default();
				self.request_load(LoadRequest::FilePath(path));
			}
		}
		let prev_file = self.folder_player.image_texture();
		let next_update = self.folder_player.update_image(&display, &mut self.image_cache);
		trace!("Folder player next update: {:?}", next_update);
		let new_file = self.folder_player.image_texture();
		let mut file_changed = prev_file.is_none() != new_file.is_none();
		if let (Some(prev), Some(new)) = (prev_file, new_file) {
			file_changed = !Rc::ptr_eq(&prev.tex_grid, &new.tex_grid);
		}
		if file_changed {
			self.image_player.start_playback_forward();
			self.image_player.request_load(LoadRequest::Jump(0));
		}
		if self.image_cache.loaded_still_image() {
			self.image_player.pause_playback();
		}
		let img_player_next_update =
			self.image_player.update_image(&display, &mut self.image_cache);
		trace!("Image player next update: {:?}", img_player_next_update);
		let next_update = next_update.aggregate(img_player_next_update);
		if self.rescanning {
			// Keep polling for the folder listing.
			let soon = Instant::now() + Duration::from_millis(50);
			return next_update.aggregate(gelatin::NextUpdate::WaitUntil(soon));
		}
		next_update
	}
}

#[derive(Debug, Clone)]
pub enum LoadedImgPath {
	NotYetLoaded,
	ErrLoading(PathBuf),
	Loaded(PathBuf),
}

impl LoadedImgPath {
	fn is_loaded(&self) -> bool {
		matches!(self, LoadedImgPath::Loaded(_))
	}
}

struct ImgSequencePlayer<P: Playback> {
	playback_state: PlaybackState,
	present_remaining: Vec<usize>,

	last_frame_change_time: Instant,
	frametime_drift_offset: i64, // in nanosecs
	load_request: LoadRequest,

	image_texture: Option<AnimationFrameTexture>,
	file_path: LoadedImgPath,

	_playback: PhantomData<P>,
}

impl<P: Playback> ImgSequencePlayer<P> {
	pub fn new() -> Self {
		ImgSequencePlayer {
			playback_state: PlaybackState::Paused,
			present_remaining: Vec::new(),
			last_frame_change_time: Instant::now(),
			frametime_drift_offset: 0,
			load_request: LoadRequest::None,
			image_texture: None,
			file_path: LoadedImgPath::NotYetLoaded,

			_playback: PhantomData,
		}
	}

	pub fn playback_state(&self) -> PlaybackState {
		self.playback_state
	}

	pub fn start_playback_forward(&mut self) {
		self.last_frame_change_time = Instant::now();
		self.frametime_drift_offset = 0;
		self.playback_state = PlaybackState::Forward;
	}

	pub fn pause_playback(&mut self) {
		self.playback_state = PlaybackState::Paused;
	}

	// Returns false if the directory hasn't finished filtering
	pub fn start_random_presentation(&mut self, image_cache: &mut ImageCache) -> bool {
		self.last_frame_change_time = Instant::now();
		self.frametime_drift_offset = 0;
		self.playback_state = PlaybackState::RandomPresent;
		self.fill_present_remainig_with_random(image_cache)
	}

	pub fn start_presentation(&mut self) {
		self.last_frame_change_time = Instant::now();
		self.frametime_drift_offset = 0;
		self.playback_state = PlaybackState::Present;
	}

	pub fn request_load(&mut self, request: LoadRequest) {
		self.load_request = request;
	}

	pub fn image_texture(&self) -> Option<AnimationFrameTexture> {
		self.image_texture.clone()
	}

	pub fn update_image(
		&mut self,
		display: &Display,
		image_cache: &mut ImageCache,
	) -> gelatin::NextUpdate {
		trace!(
			"Begin `update_image`. Curr image is: {:?}. Load request is {:?}",
			self.file_path, self.load_request
		);
		let is_paused = matches!(self.playback_state, PlaybackState::Paused);
		let no_request = matches!(self.load_request, LoadRequest::None);
		if !self.file_path.is_loaded() && no_request && is_paused {
			return gelatin::NextUpdate::Latest;
		}
		let now = Instant::now();
		let few_millisecs_from_now = now.checked_add(Duration::from_millis(50)).unwrap();
		let mut next_update;
		// The reason why I reset the `self.load_request` in such a convoluted way is that
		// it has to be guaranteed that it will be reset even if I return from this
		// function early. And at the same time I want to use it's value as it is at this line.
		let mut load_request = LoadRequest::None;
		mem::swap(&mut self.load_request, &mut load_request);

		let frame_delta_time_nanos = match self.playback_state {
			PlaybackState::Present | PlaybackState::RandomPresent => (NANOS_PER_SEC * 6) as i64,
			_ => P::delay_nanos(self) as i64,
		};
		if self.playback_state == PlaybackState::Paused {
			if let Err(e) = image_cache.process_prefetched(display) {
				error!("Failed to process prefetched images with error '{:?}'", e);
			}
			match load_request {
				LoadRequest::Jump(0) => {
					// Waiting on current image to be loaded.
					next_update = gelatin::NextUpdate::WaitUntil(few_millisecs_from_now);
				}
				_ => {
					image_cache.prefetch_neighbors();
					next_update = gelatin::NextUpdate::Latest;
				}
			}
		} else if load_request == LoadRequest::None {
			let elapsed = self.last_frame_change_time.elapsed();
			let elapsed_nanos = elapsed.as_secs() * NANOS_PER_SEC + elapsed.subsec_nanos() as u64;
			let elapsed_nanos = elapsed_nanos as i64 + self.frametime_drift_offset;

			let nanos_til_next = frame_delta_time_nanos - elapsed_nanos;
			// Wake up exactly when the next frame is due. If it is already due without a frame
			// step (zero frame delay) don't spin: look again shortly.
			let wait = if nanos_til_next > 0 {
				Duration::from_nanos(nanos_til_next as u64)
			} else {
				Duration::from_millis(1)
			};
			let next_frame_time = now.checked_add(wait).unwrap_or(few_millisecs_from_now);
			next_update = gelatin::NextUpdate::WaitUntil(next_frame_time);
			// This assumes that the following frames have the same delay but that's okay considering that
			// if frame step is greater than 1 it almost certainly means that we couldn't load the
			// next frame quiclky enough so there's not much else to do here.

			let frame_step =
				if frame_delta_time_nanos > 0 { elapsed_nanos / frame_delta_time_nanos } else { 0 };
			if frame_step > 0 {
				load_request = match self.playback_state {
					PlaybackState::Forward | PlaybackState::Present => {
						// if we can't load the frames quickly enough,
						// we won't jump over frames, but instead play the animation slower.
						LoadRequest::Jump(frame_step.min(1) as i32)
					}
					PlaybackState::RandomPresent => {
						let mut target = None;
						for _ in 0..frame_step {
							target = self.present_remaining.pop();
							if target.is_none() {
								// Restart
								// WARNING we silently assume that the folder is fully
								// filtered at this point.
								self.fill_present_remainig_with_random(image_cache);
								target = self.present_remaining.pop();
							}
						}
						match target {
							Some(index) => LoadRequest::LoadAtIndex(index),
							None => LoadRequest::None,
						}
					}
					PlaybackState::Paused => unreachable!(),
				};
				self.last_frame_change_time = Instant::now();
				self.frametime_drift_offset = -nanos_til_next;
			} else {
				if let Err(e) = image_cache.process_prefetched(display) {
					error!("Failed to process prefetched images with error '{:?}'", e);
				}
				match self.playback_state {
					PlaybackState::RandomPresent => {
						if let Some(&last) = self.present_remaining.iter().last() {
							image_cache.prefetch_at_index(last);
						}
					}
					_ => image_cache.prefetch_neighbors(),
				}
				if image_cache.has_pending_requests() {
					// Keep polling so that finished prefetches get uploaded before the frame swap.
					next_update =
						gelatin::NextUpdate::WaitUntil(next_frame_time.min(few_millisecs_from_now));
				}
			}
		} else {
			next_update = gelatin::NextUpdate::WaitUntil(few_millisecs_from_now);
		}
		match load_request {
			LoadRequest::None | LoadRequest::FilePath(..) => (),
			_ => {
				if image_cache.current_dir_len() == Some(0) {
					return gelatin::NextUpdate::Latest;
				}
			}
		}
		trace!("Attempting actual load in `update_image`");
		let load_result = match load_request {
			LoadRequest::LoadNext => Some(P::load_next(image_cache, display)),
			LoadRequest::LoadPrevious => Some(P::load_prev(image_cache, display)),
			LoadRequest::FilePath(file_path) => {
				let load_result = P::load_path(image_cache, display, &file_path);
				Some(Ok((file_path, load_result)))
			}
			LoadRequest::LoadAtIndex(index) => Some(P::load_at_index(image_cache, display, index)),
			LoadRequest::Jump(jump_count) => Some(P::load_jump(image_cache, display, jump_count)),
			LoadRequest::None => None,
		};
		if let Some(loaded_image) = load_result {
			match loaded_image {
				Ok((path, result)) => match result {
					Ok(frame) => {
						self.image_texture = Some(frame);
						self.file_path = LoadedImgPath::Loaded(path);
					}
					Err(image_cache::TextureError::WaitingOnLoader) => {
						// Set the load request to jump in place so that
						// next time we attempt to load this again.
						self.load_request = LoadRequest::Jump(0);
						next_update = gelatin::NextUpdate::WaitUntil(few_millisecs_from_now);
					}
					Err(image_cache::TextureError::WaitingOnListing) => {
						// The folder of this file is being listed. Ask for the same file again
						// (not the current one, which is still the previous folder's).
						self.load_request = LoadRequest::FilePath(path);
						next_update = gelatin::NextUpdate::WaitUntil(few_millisecs_from_now);
					}
					Err(err) => {
						self.image_texture = None;
						self.file_path = LoadedImgPath::ErrLoading(path);
						error!("Error occurred while loading image: {}", err);
					}
				},
				Err(PathResolutionError::WaitingOnDirFilter) => {
					// Set the load request to jump in place so that
					// next time we attempt to load this again.
					self.load_request = LoadRequest::Jump(0);
					next_update = gelatin::NextUpdate::WaitUntil(few_millisecs_from_now);
				}
				Err(PathResolutionError::NotYetSpecified) => {
					self.image_texture = None;
					self.file_path = LoadedImgPath::NotYetLoaded;
				}
			}
		}
		next_update
	}

	fn fill_present_remainig_with_random(&mut self, image_cache: &mut ImageCache) -> bool {
		self.present_remaining.clear();
		if let Some(dir_len) = image_cache.current_dir_len() {
			for i in 0..dir_len {
				self.present_remaining.push(i);
			}
			let mut rng = rng();
			self.present_remaining.as_mut_slice().shuffle(&mut rng);
			true
		} else {
			false
		}
	}
}
