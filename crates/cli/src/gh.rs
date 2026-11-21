//! The GitHub CLI, for what the command line leaves out: the current PR, and a token.

use std::process::Command;

use anyhow::{Context, Result, bail};

/// What `gh` prints, trimmed.
pub fn gh(args: &[&str]) -> Result<String> {
    let out = Command::new("gh")
        .args(args)
        .output()
        .context("run `gh`, the GitHub CLI")?;
    if !out.status.success() {
        bail!(
            "`gh {}`: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?.trim().to_owned())
}
