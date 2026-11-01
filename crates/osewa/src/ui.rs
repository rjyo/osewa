//! The attach UI on the local terminal: osewa-vt's, fed from stdin.

pub(crate) use osewa_vt::ui::*;

use crate::cli::Ctx;

/// Where forward_input sends what the user does.
pub(crate) trait InputHandler {
    fn send(&self, text: &str);
    fn take(&self);
    /// What the local terminal reported about itself.
    fn reports(&self, reports: &[Report]);
    fn flush(&self, ctx: &Ctx);
}

impl InputHandler for Attached {
    fn send(&self, text: &str) {
        self.type_text(text)
    }

    fn take(&self) {
        self.take_over()
    }

    fn reports(&self, reports: &[Report]) {
        self.report(reports)
    }

    fn flush(&self, ctx: &Ctx) {
        if let Some(s) = self.session() {
            let _ = osewa_client::block_on(ctx.run(s.flush()));
        }
    }
}
