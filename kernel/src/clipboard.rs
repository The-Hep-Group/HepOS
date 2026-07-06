//! System-wide text clipboard — shared by the editor and terminal so
//! Ctrl+C/Ctrl+V (and the right-click Copy/Paste menu) can move text between
//! them, not just within a single window.

use alloc::vec::Vec;
use spin::Mutex;

static CLIPBOARD: Mutex<Vec<u8>> = Mutex::new(Vec::new());

pub fn set(data: &[u8]) {
    *CLIPBOARD.lock() = Vec::from(data);
}

pub fn get() -> Vec<u8> {
    CLIPBOARD.lock().clone()
}
