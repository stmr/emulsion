use std::sync::{
	Arc, Condvar, Mutex, PoisonError,
	atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;

enum ActionState<InpT, OutT> {
	Ready,
	InputGiven(InpT),
	Pending,
	OutputReady(OutT),
}

struct Shared<InpT, OutT> {
	run: AtomicBool,
	state: Mutex<ActionState<InpT, OutT>>,
	/// Notified when input arrives or the action is being dropped.
	wake: Condvar,
}

fn run_processor<InpT, OutT, F: FnMut(InpT) -> OutT>(
	shared: Arc<Shared<InpT, OutT>>,
	mut action: F,
) {
	loop {
		let input = {
			let mut state = shared.state.lock().unwrap();
			loop {
				if !shared.run.load(Ordering::SeqCst) {
					return;
				}
				match std::mem::replace(&mut *state, ActionState::Pending) {
					ActionState::InputGiven(input) => break input,
					other => *state = other,
				}
				state = shared.wake.wait(state).unwrap();
			}
		}; // let go of the mutex lock
		let output = action(input);
		let mut state = shared.state.lock().unwrap();
		if let ActionState::Pending = &*state {
			*state = ActionState::OutputReady(output);
		}
	}
}

pub struct ParallelAction<InpT, OutT> {
	shared: Arc<Shared<InpT, OutT>>,
	join_handle: Option<JoinHandle<()>>,
}

impl<InpT: Send + 'static, OutT: Send + 'static> ParallelAction<InpT, OutT> {
	pub fn new<F: 'static + Send + FnMut(InpT) -> OutT>(action: F) -> ParallelAction<InpT, OutT> {
		let shared = Arc::new(Shared {
			run: AtomicBool::new(true),
			state: Mutex::new(ActionState::Ready),
			wake: Condvar::new(),
		});
		let handle = {
			let shared = shared.clone();
			std::thread::spawn(move || {
				run_processor(shared, action);
			})
		};

		ParallelAction { join_handle: Some(handle), shared }
	}

	/// Replaces any input that was not taken yet. If the action is running for an older input,
	/// its output is discarded.
	pub fn give_input(&self, input: InpT) {
		let mut state = self.shared.state.lock().unwrap();
		*state = ActionState::InputGiven(input);
		drop(state);
		self.shared.wake.notify_one();
	}

	pub fn try_get_output(&self) -> Option<OutT> {
		let mut state = self.shared.state.lock().unwrap();
		if let ActionState::OutputReady(_) = &*state {
			let mut output = ActionState::Ready;
			std::mem::swap(&mut output, &mut *state);
			match output {
				ActionState::OutputReady(o) => Some(o),
				_ => unreachable!(),
			}
		} else {
			None
		}
	}
}
impl<InpT, OutT> Drop for ParallelAction<InpT, OutT> {
	fn drop(&mut self) {
		self.shared.run.store(false, Ordering::SeqCst);
		// Taking the lock first guarantees the worker is either waiting or will see `run == false`.
		drop(self.shared.state.lock().unwrap_or_else(PoisonError::into_inner));
		self.shared.wake.notify_all();
		if let Some(handle) = self.join_handle.take()
			&& handle.join().is_err()
		{
			log::error!("The parallel action worker thread panicked");
		}
	}
}
