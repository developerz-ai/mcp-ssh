//! Test-only log capture shared by the "no secret in any log line" tests.
//!
//! One global subscriber, installed once, writes into a per-thread buffer.
//! Not a scoped one (`set_default` / `with_subscriber`): while a scoped
//! subscriber is being installed, a parallel test thread with no subscriber can
//! cache a callsite's interest as "never", silently dropping the very events a
//! test asserts on. With a global subscriber that window doesn't exist.
//!
//! Captures events emitted on the calling thread only — `#[tokio::test]`'s
//! current-thread runtime runs the code under test there.

use std::cell::RefCell;
use std::sync::{Arc, Mutex, Once};

/// Lines captured on one test thread since [`capture`] was called.
#[derive(Clone, Default)]
pub struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

thread_local! {
    static SINK: RefCell<Option<Captured>> = const { RefCell::new(None) };
}

/// Writes into the calling thread's sink, if a test installed one.
struct ThreadSink;

impl std::io::Write for ThreadSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        SINK.with(|sink| {
            if let Some(captured) = sink.borrow().as_ref() {
                captured.0.lock().unwrap().extend_from_slice(buf);
            }
        });
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Routes this thread's log lines (DEBUG and up, no ANSI) into a fresh buffer.
pub fn capture() -> Captured {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_ansi(false)
            .with_writer(|| ThreadSink)
            .try_init()
            .expect("no other global subscriber in unit tests");
    });
    let captured = Captured::default();
    SINK.with(|sink| *sink.borrow_mut() = Some(captured.clone()));
    captured
}
