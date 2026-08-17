//! Keyboard input path: IRQ handler → lock-free scancode queue → async
//! stream → pc-keyboard decoding.
//!
//! Privacy rule (TDD): decoded keystrokes are rendered to the local console
//! only — they never reach the serial log, which is an output channel an
//! observer could be attached to.

use core::pin::Pin;
use core::task::{Context, Poll};
use crossbeam_queue::ArrayQueue;
use futures_util::stream::{Stream, StreamExt};
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

pub struct ScancodeStream(());

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

/// Echoes decoded keys to the console. The M5 shell replaces this task.
#[cfg_attr(feature = "selftest", allow(dead_code))]
pub async fn echo_keypresses() {
    use pc_keyboard::layouts::{AnyLayout, Us104Key};
    use pc_keyboard::{DecodedKey, EventDecoder, HandleControl, ScancodeSet, ScancodeSet1};

    let mut stream = ScancodeStream::new();
    let mut scancodes = ScancodeSet1::new();
    let mut decoder = EventDecoder::new(AnyLayout::Us104Key(Us104Key), HandleControl::Ignore);
    while let Some(scancode) = stream.next().await {
        if let Ok(Some(event)) = scancodes.advance_state(scancode)
            && let Some(DecodedKey::Unicode(c)) = decoder.process_keyevent(event)
        {
            crate::console::with_console(|console| console.write_char(c));
        }
    }
}
