//! Waker-based executor. Ready task ids sit in a fixed-capacity lock-free
//! queue that wakers (including ones invoked from interrupt context) push
//! into; when nothing is ready the CPU halts until the next interrupt, so an
//! idle Osmium burns no cycles.

use super::{Task, TaskId};
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::task::Wake;
use core::task::{Context, Poll, Waker};
use crossbeam_queue::ArrayQueue;

pub struct Executor {
    tasks: BTreeMap<TaskId, Task>,
    ready_queue: Arc<ArrayQueue<TaskId>>,
    waker_cache: BTreeMap<TaskId, Waker>,
}

impl Executor {
    pub fn new() -> Self {
        Self {
            tasks: BTreeMap::new(),
            ready_queue: Arc::new(ArrayQueue::new(128)),
            waker_cache: BTreeMap::new(),
        }
    }

    pub fn spawn(&mut self, task: Task) {
        let id = task.id;
        if self.tasks.insert(id, task).is_some() {
            panic!("task id collision");
        }
        self.ready_queue
            .push(id)
            .expect("ready queue full at spawn");
    }

    pub(crate) fn run_ready_tasks(&mut self) {
        while let Some(id) = self.ready_queue.pop() {
            let Some(task) = self.tasks.get_mut(&id) else {
                continue; // task finished; a stale wake is harmless
            };
            let waker = self.waker_cache.entry(id).or_insert_with(|| {
                Waker::from(Arc::new(TaskWaker {
                    id,
                    ready_queue: self.ready_queue.clone(),
                }))
            });
            let mut context = Context::from_waker(waker);
            match task.poll(&mut context) {
                Poll::Ready(()) => {
                    self.tasks.remove(&id);
                    self.waker_cache.remove(&id);
                }
                Poll::Pending => {}
            }
        }
    }

    // The selftest battery exits before the executor loop starts.
    #[cfg_attr(feature = "selftest", allow(dead_code))]
    pub fn run(&mut self) -> ! {
        loop {
            self.run_ready_tasks();
            self.sleep_if_idle();
        }
    }

    /// The interrupts-disabled check closes the race where a wake arrives
    /// between the emptiness check and the hlt: enable_and_hlt runs sti
    /// immediately before hlt, so a pending interrupt wakes the hlt.
    #[cfg_attr(feature = "selftest", allow(dead_code))]
    fn sleep_if_idle(&self) {
        use x86_64::instructions::interrupts;
        interrupts::disable();
        if self.ready_queue.is_empty() {
            interrupts::enable_and_hlt();
        } else {
            interrupts::enable();
        }
    }
}

impl Default for Executor {
    fn default() -> Self {
        Self::new()
    }
}

struct TaskWaker {
    id: TaskId,
    ready_queue: Arc<ArrayQueue<TaskId>>,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        // A full queue drops the wake; with capacity 128 and single-digit
        // task counts this cannot happen in practice, and the task stays
        // Pending rather than corrupting anything if it ever does.
        let _ = self.ready_queue.push(self.id);
    }
}
