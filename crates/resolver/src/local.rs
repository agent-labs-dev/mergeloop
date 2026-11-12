//! An agent CLI on a developer's machine, signed in with the CLI's own login: no API key, no
//! proxy, no `--bare`, no custom Codex provider. The daemon of `mergeloop` runs it. The command
//! is the developer's to name, so a wrapper that takes the CLI's flags (as `mclaude` takes
//! `claude`'s) runs where the CLI would.
//!
//! What it may touch is `Access`: confined to a folder, or not at all. Each CLI enforces the
//! confinement itself, as checked by hand on 2026-11-12 with Claude Code 2.1.321 and Codex
//! 0.167.2 on macOS:
//! - Claude Code: its sandbox confines the shell's writes to the working directories (the
//!   worktree, and the folder through `--add-dir`) and to its own temp folder
//!   `/tmp/claude-<uid>`, which its shell needs: denying it breaks every command.
//!   `permissions.blockWritesOutsideWorkingDirectories` confines its file tools to the
//!   working directories. Nothing prompts: anything that would is denied.
//! - Codex: `--sandbox workspace-write` confines writes to the worktree and the folder through
//!   `--add-dir`, without `/tmp` or `$TMPDIR`. It keeps every `.git` read-only, so the agent
//!   cannot commit: its caller commits.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use serde_json::json;

use crate::cli::{self, Ended};
use crate::{Resolution, claude, codex};

/// Which CLI it is: it decides the flags, and how its output reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Vendor {
    Claude,
    Codex,
}

impl Vendor {
    pub fn name(self) -> &'static str {
        match self {
            Vendor::Claude => "claude",
            Vendor::Codex => "codex",
        }
    }
}

/// What the agent may touch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// Everything inside this folder, with no prompts, and no writes outside it but the CLI's
    /// own temp folder, where it needs one (see the module's doc).
    Workspace(PathBuf),
    /// The whole machine, with no prompts.
    Full,
}

impl Access {
    /// What a CLI cannot hold to in this mode, for the developer to know. `None` when it holds.
    pub fn caveat(&self, vendor: Vendor) -> Option<&'static str> {
        match (self, vendor) {
            (Access::Full, _) | (Access::Workspace(_), Vendor::Codex) => None,
            (Access::Workspace(_), Vendor::Claude) => Some(
                "Claude Code's sandbox also lets the agent's shell write to its own temp folder, \
/tmp/claude-<uid>: the shell does not run without it. On Linux it needs bubblewrap and socat; \
without them Claude Code refuses to start rather than run unconfined.",
            ),
        }
    }
}

/// One agent CLI, as the developer set it up.
#[derive(Clone, Debug)]
pub struct LocalAgent {
    pub vendor: Vendor,
    /// The program to run: the vendor's name, or a wrapper that takes its flags.
    pub command: String,
    /// `None` is the CLI's own default.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// More arguments, after MergeLoop's and before the prompt.
    pub args: Vec<String>,
    pub timeout: Duration,
    /// The CLIs have no turn limit, so the run counts them.
    pub max_turns: u32,
}

impl LocalAgent {
    /// The agent's run in `workdir`, given `prompt`. The agent's temp folder is `tmp`.
    pub fn command(
        &self,
        workdir: &Path,
        prompt: &str,
        access: &Access,
        tmp: &Path,
    ) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.command);
        cmd.current_dir(workdir).env("TMPDIR", tmp);
        match self.vendor {
            Vendor::Claude => self.claude(&mut cmd, prompt, access),
            Vendor::Codex => self.codex(&mut cmd, workdir, prompt, access),
        }
        cmd
    }

    fn claude(&self, cmd: &mut tokio::process::Command, prompt: &str, access: &Access) {
        cmd.arg("-p").arg(prompt).args([
            "--output-format",
            "stream-json",
            "--verbose",
            "--no-session-persistence",
        ]);
        if let Some(model) = &self.model {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = &self.effort {
            cmd.args(["--effort", effort]);
        }
        match access {
            Access::Workspace(folder) => {
                let settings = json!({
                    "sandbox": {
                        "enabled": true,
                        // Never run unconfined: no sandbox is no run.
                        "failIfUnavailable": true,
                        "autoAllowBashIfSandboxed": true,
                        "allowUnsandboxedCommands": false,
                        "network": { "allowedDomains": ["*"] },
                    },
                    "permissions": {
                        "blockWritesOutsideWorkingDirectories": true,
                        "denyInsteadOfPrompting": true,
                    },
                });
                cmd.args(["--permission-mode", "acceptEdits", "--add-dir"])
                    .arg(folder)
                    .arg("--settings")
                    .arg(settings.to_string());
            }
            Access::Full => _ = cmd.args(["--permission-mode", "bypassPermissions"]),
        }
        cmd.args(&self.args);
    }

    fn codex(
        &self,
        cmd: &mut tokio::process::Command,
        workdir: &Path,
        prompt: &str,
        access: &Access,
    ) {
        let toml = |v: &str| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""));
        cmd.args(["exec", "--json", "--ephemeral", "--cd"])
            .arg(workdir);
        if let Some(model) = &self.model {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = &self.effort {
            cmd.arg("-c")
                .arg(format!("model_reasoning_effort={}", toml(effort)));
        }
        match access {
            Access::Workspace(folder) => {
                cmd.args(["--sandbox", "workspace-write"])
                    .args(["-c", "approval_policy=\"never\""])
                    .args(["-c", "sandbox_workspace_write.network_access=true"])
                    .args(["-c", "sandbox_workspace_write.exclude_slash_tmp=true"])
                    .args(["-c", "sandbox_workspace_write.exclude_tmpdir_env_var=true"])
                    .arg("--add-dir")
                    .arg(folder);
            }
            Access::Full => _ = cmd.arg("--dangerously-bypass-approvals-and-sandbox"),
        }
        cmd.args(&self.args).arg(prompt);
    }

    /// Runs the agent in `workdir` with `prompt`, keeping its output in `transcript`. A run on
    /// the developer's own login bills their plan, so it has no cost. An error when it could not
    /// start.
    pub async fn run(
        &self,
        workdir: &Path,
        prompt: &str,
        access: &Access,
        tmp: &Path,
        transcript: &Path,
    ) -> Result<Resolution> {
        tokio::fs::create_dir_all(tmp).await?;
        let cmd = self.command(workdir, prompt, access, tmp);
        let name = &self.command;
        let mut resolution = match self.vendor {
            Vendor::Claude => {
                let mut run = claude::Run::default();
                let ended = cli::stream(cmd, name, self.timeout, transcript, |e| {
                    run.on(e, self.max_turns)
                })
                .await?;
                run.end(ended)
            }
            Vendor::Codex => {
                let mut run = codex::Run::default();
                let ended: Ended = cli::stream(cmd, name, self.timeout, transcript, |e| {
                    run.on(e, self.max_turns)
                })
                .await?;
                run.end(ended, None)
            }
        };
        resolution.cost_usd = None;
        Ok(resolution)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Outcome;

    fn agent(vendor: Vendor, command: &str) -> LocalAgent {
        LocalAgent {
            vendor,
            command: command.into(),
            model: None,
            effort: None,
            args: vec![],
            timeout: Duration::from_secs(20),
            max_turns: 50,
        }
    }

    fn argv(cmd: &tokio::process::Command) -> Vec<String> {
        let cmd = cmd.as_std();
        std::iter::once(cmd.get_program())
            .chain(cmd.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn has(argv: &[String], pair: &[&str]) -> bool {
        argv.windows(pair.len()).any(|w| w == pair)
    }

    #[test]
    fn the_command_is_the_one_configured_and_takes_the_vendors_flags() {
        let mut claude = agent(Vendor::Claude, "mclaude");
        claude.model = Some("haiku".into());
        claude.args = vec!["--strict-mcp-config".into()];
        let ws = Access::Workspace("/ws".into());
        let a = argv(&claude.command(Path::new("/ws/b/1"), "do it", &ws, Path::new("/ws/tmp")));
        assert_eq!(a[0], "mclaude");
        assert!(has(&a, &["-p", "do it"]), "{a:?}");
        assert!(has(&a, &["--output-format", "stream-json"]), "{a:?}");
        assert!(has(&a, &["--model", "haiku"]), "{a:?}");
        assert!(!a.iter().any(|x| x == "--effort" || x == "--bare"), "{a:?}");
        assert_eq!(a.last().unwrap(), "--strict-mcp-config");

        let codex = agent(Vendor::Codex, "codex");
        let a = argv(&codex.command(Path::new("/ws/b/1"), "do it", &ws, Path::new("/ws/tmp")));
        assert_eq!(a[..3], ["codex", "exec", "--json"]);
        assert!(has(&a, &["--cd", "/ws/b/1"]), "{a:?}");
        assert_eq!(a.last().unwrap(), "do it");
        assert!(!a.iter().any(|x| x.contains("model_provider")), "{a:?}");
    }

    #[test]
    fn workspace_access_confines_each_cli_to_the_folder_without_prompts() {
        let ws = Access::Workspace("/ws".into());
        let claude = argv(&agent(Vendor::Claude, "claude").command(
            Path::new("/ws/b/1"),
            "p",
            &ws,
            Path::new("/ws/tmp"),
        ));
        assert!(
            has(&claude, &["--permission-mode", "acceptEdits"]),
            "{claude:?}"
        );
        assert!(has(&claude, &["--add-dir", "/ws"]), "{claude:?}");
        let settings = &claude[claude.iter().position(|a| a == "--settings").unwrap() + 1];
        let settings: serde_json::Value = serde_json::from_str(settings).unwrap();
        assert_eq!(settings["sandbox"]["enabled"], true);
        assert_eq!(settings["sandbox"]["failIfUnavailable"], true);
        assert_eq!(settings["sandbox"]["allowUnsandboxedCommands"], false);
        assert_eq!(
            settings["permissions"]["blockWritesOutsideWorkingDirectories"],
            true
        );

        let codex = argv(&agent(Vendor::Codex, "codex").command(
            Path::new("/ws/b/1"),
            "p",
            &ws,
            Path::new("/ws/tmp"),
        ));
        assert!(has(&codex, &["--sandbox", "workspace-write"]), "{codex:?}");
        assert!(has(&codex, &["--add-dir", "/ws"]), "{codex:?}");
        assert!(
            has(&codex, &["-c", "approval_policy=\"never\""]),
            "{codex:?}"
        );
        assert!(
            !codex.iter().any(|a| a.contains("dangerously")),
            "{codex:?}"
        );
    }

    #[test]
    fn full_access_lifts_the_confinement() {
        let claude = argv(&agent(Vendor::Claude, "claude").command(
            Path::new("."),
            "p",
            &Access::Full,
            Path::new("/t"),
        ));
        assert!(has(&claude, &["--permission-mode", "bypassPermissions"]));
        assert!(!claude.iter().any(|a| a == "--settings" || a == "--add-dir"));
        let codex = argv(&agent(Vendor::Codex, "codex").command(
            Path::new("."),
            "p",
            &Access::Full,
            Path::new("/t"),
        ));
        assert!(codex.contains(&"--dangerously-bypass-approvals-and-sandbox".to_owned()));
        assert!(!codex.iter().any(|a| a == "--sandbox"));
    }

    /// A CLI that prints `lines` as its output, and notes the `TMPDIR` it was given.
    fn fake_cli(dir: &Path, name: &str, lines: &[serde_json::Value]) -> String {
        let out: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let data = dir.join(format!("{name}.out"));
        std::fs::write(&data, out).unwrap();
        let path = dir.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\necho \"$TMPDIR\" > {}/tmpdir\ncat {}\n",
                dir.display(),
                data.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path.display().to_string()
    }

    #[tokio::test]
    async fn a_claude_run_reads_its_answer_and_a_give_up() {
        let dir = tempfile::tempdir().unwrap();
        let command = fake_cli(
            dir.path(),
            "my-claude",
            &[
                serde_json::json!({ "type": "assistant" }),
                serde_json::json!({ "type": "result", "result": "MERGEPATROL_GIVE_UP: too vague", "is_error": false }),
            ],
        );
        let tmp = dir.path().join("tmp");
        let run = agent(Vendor::Claude, &command)
            .run(
                dir.path(),
                "p",
                &Access::Full,
                &tmp,
                &dir.path().join("t.jsonl"),
            )
            .await
            .unwrap();
        assert_eq!(run.outcome, Outcome::GaveUp("too vague".into()));
        assert_eq!(run.turns, 1);
        assert_eq!(run.cost_usd, None);
        let said = std::fs::read_to_string(dir.path().join("tmpdir")).unwrap();
        assert_eq!(said.trim(), tmp.display().to_string());
        assert!(dir.path().join("t.jsonl").exists());
    }

    #[tokio::test]
    async fn a_codex_run_reads_its_last_message() {
        let dir = tempfile::tempdir().unwrap();
        let command = fake_cli(
            dir.path(),
            "codex",
            &[
                serde_json::json!({ "type": "item.completed", "item": { "type": "agent_message", "text": "MERGELOOP_COMMIT: Add it" } }),
                serde_json::json!({ "type": "turn.completed", "usage": { "input_tokens": 10, "cached_input_tokens": 0, "output_tokens": 1 } }),
            ],
        );
        let run = agent(Vendor::Codex, &command)
            .run(
                dir.path(),
                "p",
                &Access::Full,
                &dir.path().join("tmp"),
                &dir.path().join("t.jsonl"),
            )
            .await
            .unwrap();
        assert_eq!(run.outcome, Outcome::Resolved);
        assert_eq!(run.text, "MERGELOOP_COMMIT: Add it");
        assert_eq!(run.cost_usd, None);
    }

    #[tokio::test]
    async fn a_command_that_is_not_there_is_an_error_that_names_it() {
        let dir = tempfile::tempdir().unwrap();
        let err = agent(Vendor::Claude, "no-such-agent-cli")
            .run(
                dir.path(),
                "p",
                &Access::Full,
                &dir.path().join("tmp"),
                &dir.path().join("t.jsonl"),
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("no-such-agent-cli"), "{err:#}");
    }
}
