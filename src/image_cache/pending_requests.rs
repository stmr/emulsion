use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::image_loader::*;

/// This struct is used in a map to determine the appropriate file when
/// a load result comes in. (Load results identify the file with the request id only)
/// See: `ImageCache::ongoing_requests`
struct PendingRequestInfo {
	/// Shared with the loader: set once nobody wants the result any more.
	cancelled: Arc<AtomicBool>,
	finished: bool,
	/// Estimated size of the decoded image, counted against the cache capacity while the
	/// request is in flight.
	reserved: isize,

	// I know that it would probably be faster to use a VecDeque and `drain` the elements
	// instead of moving the entire vector in the `take_results` function.
	// But after multiple attempts and a long and tiring fight with the borrow checker
	// I gave up.
	results: Vec<LoadResult>,
}

pub struct PendingRequests {
	/// Keeps track of all requests that have been sent
	/// but for which no response has been received
	by_id: HashMap<u32, PendingRequestInfo>,
}

impl PendingRequests {
	pub fn new() -> Self {
		PendingRequests { by_id: HashMap::new() }
	}

	pub fn take_results(&mut self, id: u32) -> Option<Vec<LoadResult>> {
		if let Entry::Occupied(mut entry) = self.by_id.entry(id) {
			if entry.get().finished {
				let info = entry.remove_entry().1;
				Some(info.results)
			} else {
				let mut result = Vec::with_capacity(3);
				std::mem::swap(&mut result, &mut (entry.get_mut().results));
				Some(result)
			}
		} else {
			None
		}
	}

	/// This returns all the ids including the finished item's
	pub fn get_all_ids(&self) -> Vec<u32> {
		self.by_id.keys().copied().collect()
	}

	pub fn contains(&self, id: &u32) -> bool {
		if let Some(info) = self.by_id.get(id) { !info.finished } else { false }
	}

	pub fn set_finished(&mut self, id: &u32) {
		if let Entry::Occupied(mut entry) = self.by_id.entry(*id) {
			if entry.get().results.is_empty() {
				entry.remove_entry();
			} else {
				entry.get_mut().finished = true;
			}
		}
	}

	pub fn len(&self) -> usize {
		self.iter().count()
	}

	/// Results of requests that are no longer tracked are dropped.
	pub fn add_load_result(&mut self, load_result: LoadResult) {
		if let Some(info) = self.by_id.get_mut(&load_result.req_id()) {
			info.results.push(load_result);
		}
	}

	pub fn add_request(&mut self, request: &LoadRequest, reserved: isize) {
		self.by_id.insert(
			request.req_id,
			PendingRequestInfo {
				cancelled: request.cancelled.clone(),
				finished: false,
				reserved,
				results: Vec::with_capacity(3),
			},
		);
	}

	/// Memory estimate of the requests that are still decoding and will be uploaded.
	pub fn reserved_total(&self) -> isize {
		self.iter().map(|(_, info)| info.reserved).sum()
	}

	/// Forgets the requests `keep` rejects and tells the loader to stop decoding them. Their
	/// slot and memory reservation are free at once; results that still arrive are dropped.
	pub fn retain(&mut self, mut keep: impl FnMut(u32) -> bool) {
		self.by_id.retain(|&id, info| {
			let keep = keep(id);
			if !keep {
				info.cancelled.store(true, Ordering::Relaxed);
			}
			keep
		});
	}

	/// True if there is anything left to receive or upload.
	pub fn has_work(&self) -> bool {
		!self.by_id.is_empty()
	}

	fn iter(&self) -> impl Iterator<Item = (&u32, &PendingRequestInfo)> {
		self.by_id.iter().filter(|(_, i)| !i.finished)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A cancelled request frees its slot and reservation right away and the loader is told.
	#[test]
	fn retain_cancels_rejected_requests() {
		let mut pending = PendingRequests::new();
		let kept = LoadRequest::new(1, "a".into(), true);
		let dropped = LoadRequest::new(2, "b".into(), false);
		pending.add_request(&kept, 10);
		pending.add_request(&dropped, 100);
		assert_eq!((pending.len(), pending.reserved_total()), (2, 110));

		pending.retain(|id| id == 1);
		assert_eq!((pending.len(), pending.reserved_total()), (1, 10));
		assert!(!kept.cancelled.load(Ordering::Relaxed));
		assert!(dropped.cancelled.load(Ordering::Relaxed));
		assert!(!pending.contains(&2));

		// A late result of the cancelled request is dropped, not tracked again.
		pending.add_load_result(LoadResult::Done { req_id: 2, truncated: false });
		assert!(pending.take_results(2).is_none());
	}
}
