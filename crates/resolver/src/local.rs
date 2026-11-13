//! An agent CLI on a developer's machine, signed in with the CLI's own login: no API key, no
//! proxy, no `--bare`, no custom Codex provider. The daemon of `mergeloop` runs it. The command
//! is the developer's to name, so a wrapper that takes the CLI's flags (as `mclaude` takes
//! `claude`'s) runs where the CLI would.
//!
//! What a run may touch is `Access`, around the run's `Room`. Each CLI enforces it itself, as
//! checked by hand on 2026-11-12 with Claude Code 2.1.321 and Codex 0.167.2 on macOS:
//! - Claude Code: its sandbox confines the shell's writes to the working directories (the tree,
//!   and the room through `--add-dir`), with `$TMPDIR` in the room, and to its own temp folder
//!   `/tmp/claude-<uid>`, which its shell needs: denying it breaks every command. Its
//!   `filesystem.denyRead` denies the shell the listed paths, and `Read` deny rules deny them to
//!   its file tools, through a symlink too. `permissions.blockWritesOutsideWorkingDirectories`
//!   confines its file tools' writes. Nothing prompts: anything that would is denied. Only the
//!   developer's own settings load (`--setting-sources user`): a tree's `.claude/` settings are
//!   the repo's, or a build's own, and must not loosen any of this.
//! - Codex: a permission profile, given in `-c`, lets commands read everything but the listed
//!   paths and write only the room; its file-editing tool is held to the same. The developer's
//!   `config.toml` is not read (`--ignore-user-config`; the login still is): a `sandbox_mode` in
//!   it would replace the profile with the older sandbox settings.

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

/// One run's room: the tree it works in, inside a folder of its own that also holds its temp
/// folder. In `workspace` access the room is all that the run, and what it runs, may write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Room {
    pub root: PathBuf,
    pub tree: PathBuf,
}

impl Room {
    /// The run's `TMPDIR`.
    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }
}

/// What a run may touch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Access {
    /// Writes only in the run's room, but where a CLI cannot hold to that (`caveats`), no reads
    /// of `deny_read`, and no prompts.
    Workspace { deny_read: Vec<PathBuf> },
    /// The whole machine, with no prompts.
    Full,
}

impl Access {
    /// `workspace` access that denies reads of `paths`, each by the path given and by where it
    /// really is: a sandbox matches a path after following its symlinks.
    pub fn workspace(paths: &[PathBuf]) -> Self {
        let mut deny_read = Vec::new();
        for path in paths {
            deny_read.push(path.clone());
            if let Ok(real) = path.canonicalize()
                && real != *path
            {
                deny_read.push(real);
            }
        }
        Access::Workspace { deny_read }
    }

    /// What a CLI does not hold to in this mode on this machine, for the developer to know.
    pub fn caveats(&self, vendor: Vendor) -> Vec<&'static str> {
        let Access::Workspace { .. } = self else {
            return vec![];
        };
        let mut said = match vendor {
            Vendor::Claude => vec![
                "Claude Code's shell can also write to its own temp folder, /tmp/claude-<uid>, \
which every build shares: its shell does not run without it.",
            ],
            Vendor::Codex => vec![],
        };
        if cfg!(target_os = "linux") {
            said.push(match vendor {
                Vendor::Claude => {
                    "On Linux, Claude Code's sandbox needs bubblewrap and socat, and refuses to \
start without them; MergeLoop has not checked its read denials on Linux."
                }
                Vendor::Codex => {
                    "On Linux, Codex's sandbox enforces the read denials; MergeLoop has not \
checked them on Linux."
                }
            });
        }
        if cfg!(target_os = "macos") {
            said.push(
                "Nothing stops the agent from asking the macOS keychain for what it holds, as \
your own programs can.",
            );
        }
        said
    }
}

/// What a run's environment changes of the daemon's own: the names it removes, then the names
/// it sets. The daemon decides what credentials a run may hold; the run only applies it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Env {
    pub remove: Vec<String>,
    pub set: Vec<(String, String)>,
}

impl Env {
    pub fn apply(&self, cmd: &mut tokio::process::Command) {
        for name in &self.remove {
            cmd.env_remove(name);
        }
        cmd.envs(self.set.iter().map(|(k, v)| (k, v)));
    }
}

/// The name of the Codex permission profile a `workspace` run gets.
const CODEX_PROFILE: &str = "mergeloop-build";

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
    /// The agent's run in `room`'s tree, given `prompt`, with `env`. Its temp folder is the
    /// room's.
    pub fn command(
        &self,
        room: &Room,
        prompt: &str,
        access: &Access,
        env: &Env,
    ) -> tokio::process::Command {
        let mut cmd = tokio::process::Command::new(&self.command);
        env.apply(&mut cmd);
        // Its own process group, so that whatever it leaves running can be stopped with it.
        cmd.current_dir(&room.tree)
            .env("TMPDIR", room.tmp())
            .process_group(0);
        match self.vendor {
            Vendor::Claude => self.claude(&mut cmd, room, prompt, access),
            Vendor::Codex => self.codex(&mut cmd, room, prompt, access),
        }
        cmd
    }

    fn claude(
        &self,
        cmd: &mut tokio::process::Command,
        room: &Room,
        prompt: &str,
        access: &Access,
    ) {
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
            Access::Workspace { deny_read } => {
                let paths: Vec<String> =
                    deny_read.iter().map(|p| p.display().to_string()).collect();
                // A permission rule's absolute path starts with `//`.
                let rules: Vec<String> = paths
                    .iter()
                    .flat_map(|p| [format!("Read(/{p})"), format!("Read(/{p}/**)")])
                    .collect();
                let settings = json!({
                    "sandbox": {
                        "enabled": true,
                        // Never run unconfined: no sandbox is no run.
                        "failIfUnavailable": true,
                        "autoAllowBashIfSandboxed": true,
                        "allowUnsandboxedCommands": false,
                        "network": { "allowedDomains": ["*"] },
                        "filesystem": { "denyRead": paths },
                    },
                    "permissions": {
                        "blockWritesOutsideWorkingDirectories": true,
                        "denyInsteadOfPrompting": true,
                        "deny": rules,
                    },
                });
                cmd.args(["--setting-sources", "user"])
                    .args(["--permission-mode", "acceptEdits", "--add-dir"])
                    .arg(&room.root)
                    .arg("--settings")
                    .arg(settings.to_string());
            }
            Access::Full => _ = cmd.args(["--permission-mode", "bypassPermissions"]),
        }
        cmd.args(&self.args);
    }

    fn codex(&self, cmd: &mut tokio::process::Command, room: &Room, prompt: &str, access: &Access) {
        let toml = |v: &str| format!("\"{}\"", v.replace('\\', "\\\\").replace('"', "\\\""));
        cmd.args(["exec", "--json", "--ephemeral", "--cd"])
            .arg(&room.tree);
        if let Some(model) = &self.model {
            cmd.args(["--model", model]);
        }
        if let Some(effort) = &self.effort {
            cmd.arg("-c")
                .arg(format!("model_reasoning_effort={}", toml(effort)));
        }
        match access {
            Access::Workspace { deny_read } => {
                let path = |p: &Path| toml(&p.display().to_string());
                let mut rules = vec![
                    format!("{} = \"read\"", toml(":root")),
                    format!("{} = \"write\"", path(&room.root)),
                ];
                rules.extend(deny_read.iter().map(|p| format!("{} = \"deny\"", path(p))));
                cmd.arg("--ignore-user-config")
                    .args([
                        "-c",
                        &format!("default_permissions={}", toml(CODEX_PROFILE)),
                    ])
                    .arg("-c")
                    .arg(format!(
                        "permissions.{CODEX_PROFILE}.filesystem={{{}}}",
                        rules.join(", ")
                    ))
                    .arg("-c")
                    .arg(format!("permissions.{CODEX_PROFILE}.network.enabled=true"))
                    .args(["-c", "approval_policy=\"never\""]);
            }
            Access::Full => _ = cmd.arg("--dangerously-bypass-approvals-and-sandbox"),
        }
        cmd.args(&self.args).arg(prompt);
    }

    /// Runs the agent in `room` with `prompt` and `env`, keeping its output in `transcript`. A run on
    /// the developer's own login bills their plan, so it has no cost. An error when it could not
    /// start.
    pub async fn run(
        &self,
        room: &Room,
        prompt: &str,
        access: &Access,
        env: &Env,
        transcript: &Path,
        spawned: impl FnOnce(u32),
    ) -> Result<Resolution> {
        tokio::fs::create_dir_all(room.tmp()).await?;
        let cmd = self.command(room, prompt, access, env);
        let name = &self.command;
        let mut resolution = match self.vendor {
            Vendor::Claude => {
                let mut run = claude::Run::default();
                let ended = cli::stream(
                    cmd,
                    name,
                    self.timeout,
                    transcript,
                    |e| run.on(e, self.max_turns),
                    spawned,
                )
                .await?;
                run.end(ended)
            }
            Vendor::Codex => {
                let mut run = codex::Run::default();
                let ended: Ended = cli::stream(
                    cmd,
                    name,
                    self.timeout,
                    transcript,
                    |e| run.on(e, self.max_turns),
                    spawned,
                )
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

    fn room() -> Room {
        Room {
            root: "/ws/builds/1".into(),
            tree: "/ws/builds/1/tree".into(),
        }
    }

    fn workspace() -> Access {
        Access::Workspace {
            deny_read: vec!["/home/ana/.ssh".into(), "/home/ana/.netrc".into()],
        }
    }

    #[test]
    fn the_command_is_the_one_configured_and_takes_the_vendors_flags() {
        let mut claude = agent(Vendor::Claude, "mclaude");
        claude.model = Some("haiku".into());
        claude.args = vec!["--strict-mcp-config".into()];
        let cmd = claude.command(&room(), "do it", &workspace(), &Env::default());
        let a = argv(&cmd);
        assert_eq!(a[0], "mclaude");
        assert_eq!(
            cmd.as_std().get_current_dir(),
            Some(Path::new("/ws/builds/1/tree"))
        );
        assert!(has(&a, &["-p", "do it"]), "{a:?}");
        assert!(has(&a, &["--output-format", "stream-json"]), "{a:?}");
        assert!(has(&a, &["--model", "haiku"]), "{a:?}");
        assert!(!a.iter().any(|x| x == "--effort" || x == "--bare"), "{a:?}");
        assert_eq!(a.last().unwrap(), "--strict-mcp-config");

        let codex = agent(Vendor::Codex, "codex");
        let a = argv(&codex.command(&room(), "do it", &workspace(), &Env::default()));
        assert_eq!(a[..3], ["codex", "exec", "--json"]);
        assert!(has(&a, &["--cd", "/ws/builds/1/tree"]), "{a:?}");
        assert_eq!(a.last().unwrap(), "do it");
        assert!(!a.iter().any(|x| x.contains("model_provider")), "{a:?}");
    }

    #[test]
    fn a_run_holds_only_the_credentials_its_env_gives() {
        let env = Env {
            remove: vec!["GH_TOKEN".into()],
            set: vec![("GITHUB_TOKEN".into(), "test-bot".into())],
        };
        let cmd = agent(Vendor::Claude, "claude").command(&room(), "p", &Access::Full, &env);
        let envs: Vec<(String, Option<String>)> = cmd
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                let s = |x: &std::ffi::OsStr| x.to_string_lossy().into_owned();
                (s(k), v.map(s))
            })
            .collect();
        assert!(envs.contains(&("GH_TOKEN".into(), None)), "{envs:?}");
        assert!(
            envs.contains(&("GITHUB_TOKEN".into(), Some("test-bot".into()))),
            "{envs:?}"
        );
    }

    #[test]
    fn workspace_access_lets_claude_write_only_its_room_and_read_nothing_denied() {
        let a = argv(&agent(Vendor::Claude, "claude").command(
            &room(),
            "p",
            &workspace(),
            &Env::default(),
        ));
        assert!(has(&a, &["--permission-mode", "acceptEdits"]), "{a:?}");
        assert!(has(&a, &["--add-dir", "/ws/builds/1"]), "{a:?}");
        assert!(has(&a, &["--setting-sources", "user"]), "{a:?}");
        let settings = &a[a.iter().position(|x| x == "--settings").unwrap() + 1];
        let settings: serde_json::Value = serde_json::from_str(settings).unwrap();
        assert_eq!(settings["sandbox"]["enabled"], true);
        assert_eq!(settings["sandbox"]["failIfUnavailable"], true);
        assert_eq!(settings["sandbox"]["allowUnsandboxedCommands"], false);
        assert_eq!(
            settings["sandbox"]["filesystem"]["denyRead"],
            json!(["/home/ana/.ssh", "/home/ana/.netrc"])
        );
        assert_eq!(
            settings["permissions"]["deny"],
            json!([
                "Read(//home/ana/.ssh)",
                "Read(//home/ana/.ssh/**)",
                "Read(//home/ana/.netrc)",
                "Read(//home/ana/.netrc/**)"
            ])
        );
        assert_eq!(
            settings["permissions"]["blockWritesOutsideWorkingDirectories"],
            true
        );
    }

    #[test]
    fn workspace_access_gives_codex_a_profile_that_writes_only_its_room() {
        let a = argv(&agent(Vendor::Codex, "codex").command(
            &room(),
            "p",
            &workspace(),
            &Env::default(),
        ));
        assert!(a.contains(&"--ignore-user-config".to_owned()), "{a:?}");
        assert!(
            has(&a, &["-c", "default_permissions=\"mergeloop-build\""]),
            "{a:?}"
        );
        assert!(
            has(
                &a,
                &[
                    "-c",
                    "permissions.mergeloop-build.filesystem={\":root\" = \"read\", \
\"/ws/builds/1\" = \"write\", \"/home/ana/.ssh\" = \"deny\", \"/home/ana/.netrc\" = \"deny\"}"
                ]
            ),
            "{a:?}"
        );
        assert!(has(&a, &["-c", "approval_policy=\"never\""]), "{a:?}");
        assert!(
            !a.iter()
                .any(|x| x.contains("dangerously") || x == "--sandbox" || x == "--add-dir"),
            "{a:?}"
        );
    }

    #[test]
    fn a_denied_path_is_denied_where_it_really_is_too() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().canonicalize().unwrap().join("dotfiles-ssh");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().canonicalize().unwrap().join(".ssh");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let Access::Workspace { deny_read } = Access::workspace(std::slice::from_ref(&link)) else {
            unreachable!()
        };
        assert_eq!(deny_read, [link, real]);
    }

    #[test]
    fn full_access_lifts_the_confinement() {
        let claude = argv(&agent(Vendor::Claude, "claude").command(
            &room(),
            "p",
            &Access::Full,
            &Env::default(),
        ));
        assert!(has(&claude, &["--permission-mode", "bypassPermissions"]));
        assert!(!claude.iter().any(|a| a == "--settings" || a == "--add-dir"));
        let codex = argv(&agent(Vendor::Codex, "codex").command(
            &room(),
            "p",
            &Access::Full,
            &Env::default(),
        ));
        assert!(codex.contains(&"--dangerously-bypass-approvals-and-sandbox".to_owned()));
        assert!(
            !codex
                .iter()
                .any(|a| a == "--sandbox" || a.contains("permissions"))
        );
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
        let room = Room {
            root: dir.path().to_owned(),
            tree: dir.path().to_owned(),
        };
        let tmp = room.tmp();
        let run = agent(Vendor::Claude, &command)
            .run(
                &room,
                "p",
                &Access::Full,
                &Env::default(),
                &dir.path().join("t.jsonl"),
                |_| {},
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
        let room = Room {
            root: dir.path().to_owned(),
            tree: dir.path().to_owned(),
        };
        let run = agent(Vendor::Codex, &command)
            .run(
                &room,
                "p",
                &Access::Full,
                &Env::default(),
                &dir.path().join("t.jsonl"),
                |_| {},
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
        let room = Room {
            root: dir.path().to_owned(),
            tree: dir.path().to_owned(),
        };
        let err = agent(Vendor::Claude, "no-such-agent-cli")
            .run(
                &room,
                "p",
                &Access::Full,
                &Env::default(),
                &dir.path().join("t.jsonl"),
                |_| {},
            )
            .await
            .unwrap_err();
        assert!(format!("{err:#}").contains("no-such-agent-cli"), "{err:#}");
    }
}
