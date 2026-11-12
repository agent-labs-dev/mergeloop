//! Runs an agent CLI that prints one JSON event per line, and keeps every line as its transcript.

use std::path::Path;
use std::process::{ExitStatus, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

/// Sends `cmd`'s calls to `url` around the job's egress proxy. Claude Code sends plain HTTP
/// through `HTTPS_PROXY` too, and the proxy only lets out the hosts on its allowlist.
pub(crate) fn bypass_proxy(cmd: &mut Command, url: &str) {
    let Some(host) = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
    else {
        return;
    };
    let hosts = match std::env::var("NO_PROXY").or_else(|_| std::env::var("no_proxy")) {
        Ok(hosts) if !hosts.is_empty() => format!("{hosts},{host}"),
        _ => host,
    };
    cmd.env("NO_PROXY", &hosts).env("no_proxy", hosts);
}

/// How a CLI run ended, once it started.
pub(crate) enum Ended {
    /// The CLI exited by itself, with its stderr.
    Exited(ExitStatus, String),
    /// It was killed: `on_event` stopped it, or it ran out of time. Why.
    Stopped(String),
}

/// How long the output of a CLI that exited is still read: what it left running may hold it
/// open.
const DRAIN: Duration = Duration::from_secs(2);

/// Runs `cmd`, handing each event to `on_event`, which stops the run with an error. `spawned`
/// gets the CLI's pid as it starts. An error when the CLI could not start or be read.
pub(crate) async fn stream(
    mut cmd: Command,
    name: &str,
    timeout: Duration,
    transcript_path: &Path,
    mut on_event: impl FnMut(&Value) -> Result<()>,
    spawned: impl FnOnce(u32),
) -> Result<Ended> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawn {name}"))?;
    if let Some(pid) = child.id() {
        spawned(pid);
    }
    let stdout = child.stdout.take().context("stdout")?;
    // Drain stderr as it comes, so a full pipe never blocks the CLI.
    let mut stderr = child.stderr.take().context("stderr")?;
    let mut stderr = tokio::spawn(async move {
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err).await;
        err
    });
    if let Some(dir) = transcript_path.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let mut transcript = tokio::fs::File::create(transcript_path).await?;

    // The outer error is reading the CLI, the inner one `on_event` stopping it. Reading ends at
    // the end of the output, or `DRAIN` after the CLI exited.
    let mut exited = None;
    let read = async {
        let mut lines = BufReader::new(stdout).lines();
        let mut drain = None;
        loop {
            let line = match drain {
                None => tokio::select! {
                    line = lines.next_line() => line?,
                    status = child.wait() => {
                        exited = Some(status?);
                        drain = Some(tokio::time::Instant::now() + DRAIN);
                        continue;
                    }
                },
                Some(until) => tokio::time::timeout_at(until, lines.next_line())
                    .await
                    .unwrap_or(Ok(None))?,
            };
            let Some(line) = line else {
                return anyhow::Ok(Ok(()));
            };
            transcript.write_all(line.as_bytes()).await?;
            transcript.write_all(b"\n").await?;
            if let Ok(event) = serde_json::from_str::<Value>(&line)
                && let Err(e) = on_event(&event)
            {
                return Ok(Err(e));
            }
        }
    };
    match tokio::time::timeout(timeout, read).await {
        Ok(Ok(Ok(()))) => {}
        Ok(Ok(Err(e))) => return Ok(Ended::Stopped(format!("{e:#}"))),
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Ok(Ended::Stopped(format!(
                "{name} did not finish in {timeout:?}"
            )));
        }
    }
    let status = match exited {
        Some(status) => status,
        None => child.wait().await?,
    };
    let err = match tokio::time::timeout(DRAIN, &mut stderr).await {
        Ok(err) => err?,
        Err(_) => {
            stderr.abort();
            String::new()
        }
    };
    Ok(Ended::Exited(status, err))
}
