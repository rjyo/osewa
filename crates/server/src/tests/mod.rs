//! Server tests: a real server on the test's thread driven by in-memory
//! client connections, with real holders on their own threads or fake ones
//! on the server's. Ported from the Go server's tests.

// Shared by every ported test file; not each uses all of it.
#[allow(dead_code)]
mod helpers;
mod channel;
mod net;
mod relay;
mod server;
mod trace;

/// The environment is process-wide; tests that change it take turns.
static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
