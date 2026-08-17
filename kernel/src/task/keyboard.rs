//! Keyboard input path: IRQ handler → lock-free scancode queue → async
//! stream → pc-keyboard decoding.
//!
//! Privacy rule (TDD): decoded keystrokes are rendered to the local console
//! only — they never reach the serial log, which is an output channel an
//! observer could be attached to.

use core::pin::Pin;
use core::task::{Context, Poll};
use crossbeam_queue::ArrayQueue;
use futures_util::stream::Stream;
use futures_util::task::AtomicWaker;
use spin::Once;

static SCANCODE_QUEUE: Once<ArrayQueue<u8>> = Once::new();
static WAKER: AtomicWaker = AtomicWaker::new();

/// Called from the keyboard interrupt handler: never blocks, never
/// allocates. Scancodes arriving before the stream exists are dropped —
/// there is nobody to type during early boot.
pub(crate) fn enqueue_scancode(scancode: u8) {
    if let Some(queue) = SCANCODE_QUEUE.get()
        && queue.push(scancode).is_ok()
    {
        WAKER.wake();
    }
}

// Only the shell constructs the stream, and the selftest build has no shell.
#[cfg_attr(feature = "selftest", allow(dead_code))]
pub struct ScancodeStream(());

#[cfg_attr(feature = "selftest", allow(dead_code))]
impl ScancodeStream {
    pub fn new() -> Self {
        SCANCODE_QUEUE.call_once(|| ArrayQueue::new(128));
        ScancodeStream(())
    }
}

impl Default for ScancodeStream {
    fn default() -> Self {
        Self::new()
    }
}

impl Stream for ScancodeStream {
    type Item = u8;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<u8>> {
        let queue = SCANCODE_QUEUE.get().expect("stream constructed first");
        if let Some(scancode) = queue.pop() {
            return Poll::Ready(Some(scancode));
        }
        WAKER.register(cx.waker());
        // Re-check after registering: an IRQ between the first pop and the
        // register would otherwise leave a scancode stranded until the next.
        match queue.pop() {
            Some(scancode) => {
                WAKER.take();
                Poll::Ready(Some(scancode))
            }
            None => Poll::Pending,
        }
    }
}
