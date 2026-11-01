//! osewa's C API, `include/osewa.h`: one remote session drawn as VT bytes
//! for an app's own terminal (osewa-vt's re-rendering, echo prediction
//! over it), kept connected and resumed by a Link. Built as a dynamic
//! library (Osewa.framework on iOS) exporting only `osewa_*`: the
//! libghostty it links stays private, beside whatever libghostty the app
//! has of its own.
//!
//! Every entry point catches panics, so none unwinds into C; the iOS build
//! unwinds (scripts/build-ios-framework.sh) for that to matter.
//!
//! Threads: calls on a handle may come from any thread, but the caller
//! must not make any concurrently with, or after, osewa_close/osewa_kill
//! on it from another thread. Only calls from inside the handle's own
//! callbacks are safe while it closes: close waits a running callback out
//! before freeing (osewa.h).

use std::ffi::{c_char, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::OnceLock;

mod options;
mod session;

pub use options::OpenOptions;
pub use session::{Callbacks, CLOSED, CONNECTED, CONNECTING, INPUT_MAX, RECONNECTING};

use session::{Handle, Refused};

/// osewa_send and osewa_paste's refusals (osewa.h).
pub const ERR: i32 = -1;
pub const ERR_FULL: i32 = -2;

fn refused(r: Result<(), Refused>) -> i32 {
    match r {
        Ok(()) => 0,
        Err(Refused::Closed) => ERR,
        Err(Refused::Full) => ERR_FULL,
    }
}

#[cfg(test)]
mod tests;

/// osewa_session: what osewa_open returns.
pub struct OsewaSession(Handle);

fn guard<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(fallback)
}

fn into_c(s: &str) -> *mut c_char {
    CString::new(s.replace('\0', "")).unwrap_or_default().into_raw()
}

/// Opens a session: connects in the background and reports through cb.
///
/// # Safety
/// opts points to a valid osewa_open_options; err is NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn osewa_open(opts: *const OpenOptions, cb: Callbacks, err: *mut *mut c_char) -> *mut OsewaSession {
    guard(std::ptr::null_mut(), || {
        let fail = |msg: &str| {
            if !err.is_null() {
                unsafe { *err = into_c(msg) };
            }
            std::ptr::null_mut()
        };
        let Some(o) = (unsafe { opts.as_ref() }) else { return fail("osewa: no options") };
        match unsafe { options::read(o) } {
            Ok(o) => Box::into_raw(Box::new(OsewaSession(Handle::open(o, cb)))),
            Err(e) => fail(&e),
        }
    })
}

/// Sends what the app's key encoder made.
///
/// # Safety
/// s is from osewa_open and not yet closed; buf holds len bytes.
#[no_mangle]
pub unsafe extern "C" fn osewa_send(s: *mut OsewaSession, buf: *const u8, len: usize) -> i32 {
    guard(-1, || {
        let Some(s) = (unsafe { s.as_ref() }) else { return -1 };
        if len == 0 {
            return 0;
        }
        if buf.is_null() {
            return -1;
        }
        let b = unsafe { std::slice::from_raw_parts(buf, len) };
        refused(s.0.send(b))
    })
}

/// Pastes text; the holder brackets it when the program asked for that.
///
/// # Safety
/// s is from osewa_open and not yet closed; utf8 is NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn osewa_paste(s: *mut OsewaSession, utf8: *const c_char) -> i32 {
    guard(-1, || {
        let Some(s) = (unsafe { s.as_ref() }) else { return -1 };
        if utf8.is_null() {
            return -1;
        }
        let Ok(text) = unsafe { CStr::from_ptr(utf8) }.to_str() else { return -1 };
        refused(s.0.paste(text.to_string()))
    })
}

/// # Safety
/// s is from osewa_open and not yet closed.
#[no_mangle]
pub unsafe extern "C" fn osewa_resize(s: *mut OsewaSession, cols: u16, rows: u16, width_px: u32, height_px: u32) {
    guard((), || {
        if let Some(s) = unsafe { s.as_ref() } {
            s.0.resize(cols, rows, width_px, height_px);
        }
    })
}

/// # Safety
/// s is from osewa_open and not yet closed.
#[no_mangle]
pub unsafe extern "C" fn osewa_redraw(s: *mut OsewaSession) {
    guard((), || {
        if let Some(s) = unsafe { s.as_ref() } {
            s.0.redraw();
        }
    })
}

/// # Safety
/// s is from osewa_open and not yet closed.
#[no_mangle]
pub unsafe extern "C" fn osewa_network_changed(s: *mut OsewaSession) {
    guard((), || {
        if let Some(s) = unsafe { s.as_ref() } {
            s.0.network_changed();
        }
    })
}

/// Quiesces the callbacks through a shared reference, then frees s: a
/// callback still running may be calling osewa_send on it meanwhile.
///
/// # Safety
/// s is from osewa_open and not yet closed.
unsafe fn close(s: *mut OsewaSession, kill: bool) {
    guard((), || {
        let Some(h) = (unsafe { s.as_ref() }) else { return };
        h.0.quiesce();
        unsafe { Box::from_raw(s) }.0.close(kill);
    })
}

/// # Safety
/// s is from osewa_open and not yet closed; it is freed.
#[no_mangle]
pub unsafe extern "C" fn osewa_close(s: *mut OsewaSession) {
    unsafe { close(s, false) }
}

/// # Safety
/// s is from osewa_open and not yet closed; it is freed.
#[no_mangle]
pub unsafe extern "C" fn osewa_kill(s: *mut OsewaSession) {
    unsafe { close(s, true) }
}

/// Writes a new device key, PKCS#8 DER, into out. When cap is too small
/// nothing is written, *len says how much is needed, and it returns -1.
///
/// # Safety
/// out holds cap bytes; len is writable.
#[no_mangle]
pub unsafe extern "C" fn osewa_generate_key(out: *mut u8, cap: usize, len: *mut usize) -> i32 {
    guard(-1, || {
        if len.is_null() {
            return -1;
        }
        let id = osewa_auth::Identity::generate();
        let der = id.pkcs8();
        unsafe { *len = der.len() };
        if out.is_null() || cap < der.len() {
            return -1;
        }
        unsafe { std::ptr::copy_nonoverlapping(der.as_ptr(), out, der.len()) };
        0
    })
}

/// The device key's public half as enroll takes it: base64 of 32 bytes.
///
/// # Safety
/// der holds len bytes.
#[no_mangle]
pub unsafe extern "C" fn osewa_public_key(der: *const u8, len: usize) -> *mut c_char {
    guard(std::ptr::null_mut(), || {
        if der.is_null() || len == 0 {
            return std::ptr::null_mut();
        }
        let der = unsafe { std::slice::from_raw_parts(der, len) }.to_vec();
        match osewa_auth::Identity::from_pkcs8(der) {
            Ok(id) => into_c(&osewa_auth::encode_key(&id.public())),
            Err(_) => std::ptr::null_mut(),
        }
    })
}

/// This build, as `osewa version --json` prints it; never freed.
#[no_mangle]
pub extern "C" fn osewa_build_info() -> *const c_char {
    static INFO: OnceLock<CString> = OnceLock::new();
    guard(c"{}".as_ptr(), || INFO.get_or_init(|| CString::new(osewa_client::build_info()).unwrap_or_default()).as_ptr())
}

/// Frees a string osewa returned (an error, a public key).
///
/// # Safety
/// s is NULL or from osewa, not yet freed.
#[no_mangle]
pub unsafe extern "C" fn osewa_string_free(s: *mut c_char) {
    guard((), || {
        if !s.is_null() {
            drop(unsafe { CString::from_raw(s) });
        }
    })
}
