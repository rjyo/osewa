//! `osewa attach`: ported separately; this is its entry point, with the
//! argument checks the other commands' tests rely on.

use crate::cli::{Ctx, Env, Error, Flags, Kind, Result};

/// Runs `attach [--take] [--quiet] [--predict auto|always|never] <host>
/// [id]` and returns the exit code.
pub(crate) async fn attach(ctx: &Ctx, env: &Env, args: &[String]) -> Result<i32> {
    let _ = ctx;
    let mut fs = Flags::new(&[("take", Kind::Bool), ("quiet", Kind::Bool), ("predict", Kind::Str)]);
    let pos = fs.parse(args)?;
    if pos.is_empty() || pos.len() > 2 {
        return Err(Error::Usage);
    }
    osewa_client::hosts::find(&env.dir, &pos[0])?;
    Err("attach is not in the Rust build yet".into())
}
