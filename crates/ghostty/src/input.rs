use crate::{check, ffi, Error, Result, Terminal};

/// A set of key modifiers, in libghostty's bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods(pub u16);

/// Converts protocol modifier names.
pub fn parse_mods<S: AsRef<str>>(names: &[S]) -> Result<Mods> {
    let (mut shift, mut ctrl, mut alt, mut sup) = (false, false, false, false);
    for n in names {
        match n.as_ref() {
            "shift" => shift = true,
            "ctrl" => ctrl = true,
            "alt" => alt = true,
            "super" => sup = true,
            _ => return Err(Error { what: "unknown modifier", code: -2 }),
        }
    }
    Ok(Mods(unsafe { ffi::osewa_mods(shift, ctrl, alt, sup) }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyAction {
    #[default]
    Press,
    Repeat,
    Release,
}

/// One key event, named by its W3C KeyboardEvent.code.
#[derive(Debug, Clone, Default)]
pub struct Key {
    pub code: String,
    pub mods: Mods,
    pub text: String,
    pub action: KeyAction,
}

fn key_code(code: &str) -> Option<i32> {
    let k = unsafe { ffi::osewa_key_lookup(code.as_ptr(), code.len()) };
    (k >= 0).then_some(k)
}

/// Whether code is a W3C code libghostty knows.
pub fn known_key(code: &str) -> bool {
    key_code(code).is_some()
}

// Calls an encoder that reports the size it needs when the buffer is short.
fn encode(initial: usize, what: &'static str, mut f: impl FnMut(&mut [u8], &mut usize) -> i32) -> Result<Vec<u8>> {
    let mut buf = vec![0u8; initial];
    let mut n = 0usize;
    let mut r = f(&mut buf, &mut n);
    if r == unsafe { ffi::osewa_out_of_space() } {
        buf = vec![0u8; n.max(1)];
        r = f(&mut buf, &mut n);
    }
    check(r, what)?;
    buf.truncate(n);
    Ok(buf)
}

impl Terminal {
    /// Encodes a key event for the terminal's current modes (cursor key
    /// mode, kitty keyboard flags, modifyOtherKeys and so on). An event the
    /// terminal reports nothing for, such as a bare release, encodes to
    /// nothing.
    pub fn encode_key(&self, k: &Key) -> Result<Vec<u8>> {
        let code = key_code(&k.code).ok_or(Error { what: "unknown key", code: -2 })?;
        let action = match k.action {
            KeyAction::Press => 0,
            KeyAction::Repeat => 1,
            KeyAction::Release => 2,
        };
        // Kitty keyboard reporting needs the base key: ASCII letters
        // lowercased.
        let mut chars = k.text.chars();
        let unshifted = match (chars.next(), chars.next()) {
            (Some(c), None) => c.to_ascii_lowercase() as u32,
            _ => 0,
        };
        let t = self.raw();
        encode(128, "key_encode", |buf, n| unsafe {
            ffi::osewa_encode_key(t, code, k.mods.0, action, k.text.as_ptr(), k.text.len(), unshifted, buf.as_mut_ptr(), buf.len(), n)
        })
    }

    /// Encodes a focus change, or nothing when the program hasn't enabled
    /// focus reporting (mode 1004).
    pub fn encode_focus(&self, focused: bool) -> Result<Vec<u8>> {
        if !self.mode(1004, false) {
            return Ok(Vec::new());
        }
        encode(16, "focus_encode", |buf, n| unsafe { ffi::osewa_encode_focus(focused, buf.as_mut_ptr(), buf.len(), n) })
    }
}
