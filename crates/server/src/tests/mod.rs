//! Server tests: a real server on the test's thread driven by in-memory
//! client connections, with real holders on their own threads or fake ones
//! on the server's.

// Shared by every test file; not each uses all of it.
#[allow(dead_code)]
mod helpers;
mod channel;
mod net;
mod quic;
mod relay;
mod server;
mod trace;

/// The environment is process-wide; tests that change it take turns.
static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Sets junk in this process's environment, as whatever started a server
/// leaves (a multiplexer's nesting guard, an agent's token, its $SHELL),
/// until dropped; holds ENV meanwhile.
pub(crate) struct Junk(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>, Vec<(&'static str, Option<std::ffi::OsString>)>);

pub(crate) const JUNK: [(&str, &str); 3] = [("HERDR_ENV", "1"), ("OSEWA_TEST_TOKEN", "secret"), ("SHELL", "/bin/false")];

impl Junk {
    pub fn set() -> Junk {
        let g = ENV.lock().unwrap_or_else(|e| e.into_inner());
        let saved = JUNK.iter().map(|(k, _)| (*k, std::env::var_os(k))).collect();
        for (k, v) in JUNK {
            unsafe { std::env::set_var(k, v) };
        }
        Junk(g, saved)
    }
}

impl Drop for Junk {
    fn drop(&mut self) {
        for (k, v) in self.1.drain(..) {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
    }
}
