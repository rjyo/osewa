//! osewa-vt's tests: drawing, the output filter and the UI, against the
//! linked libghostty.

use std::io::Write;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod render;
mod ui;
mod vtfilter;

/// A buffer the UI writes to and the test reads, from any thread.
#[derive(Clone, Default)]
pub(crate) struct Buf(Arc<Mutex<Vec<u8>>>);

impl Write for Buf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Buf {
    pub fn string(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}
