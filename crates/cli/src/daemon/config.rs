//! The daemon's config file, local to the machine: `~/.mergeloop/daemon.toml`, or `--config`.
//! A missing file is the defaults; a bad one is an error that says where.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use mergepatrol_resolver::{Access, LocalAgent, Vendor};
use serde::Deserialize;

/// The most turns an agent's run may take. The CLIs have no limit of their own.
const MAX_TURNS: u32 = 400;

/// What `mergeloop daemon --help` says of the file: every setting, with its default.
pub const SETTINGS: &str = "\
Config file (~/.mergeloop/daemon.toml, or --config FILE; no file means these defaults):

  slots = 1                  How many builds run at once
  workspace = \"~/.mergeloop/daemon\"
                             The daemon's folder: its clone of each repo, a worktree per
                             build, the agents' transcripts (kept 7 days)
  access = \"workspace\"       \"workspace\": without prompts, the agent, what it runs and
                             the verify commands may write only the build's own folder in
                             the workspace, and may not read the deny_read paths.
                             \"full\": no confinement
  deny_read = [...]          What a build may not read, in workspace access. Default: the
                             MergeLoop session (~/.mergeloop/session) and where credentials
                             live in a home folder: ~/.ssh, ~/.aws, ~/.gnupg, ~/.netrc,
                             ~/.git-credentials, ~/.config/gh, ~/.config/gcloud, ~/.azure,
                             ~/.docker/config.json, ~/.kube, ~/.npmrc, ~/.cargo/credentials,
                             ~/.cargo/credentials.toml, ~/.pypirc, ~/.gem/credentials,
                             ~/.codex/auth.json, ~/.claude/.credentials.json. A list here
                             replaces it

  [builder]                  The agent that builds
  agent = \"claude\"           \"claude\" or \"codex\": the flags it gets, the output read
  command = \"<agent>\"        The program to run, such as a wrapper that takes the agent's
                             flags (\"mclaude\")
  model = \"\"                 Default: the CLI's own
  effort = \"\"                Default: the CLI's own
  args = []                  More arguments for every run
  timeout_minutes = 60       The longest one agent run may take

  [reviewer]                 The agent that reviews each build before it is pushed, with
                             the builder's settings above, read-only
  enabled = true             false: no review
  agent = \"<the other>\"      Default: the other agent when its CLI is on PATH, else (or
                             when command is set) the builder's
  command = \"<agent>\"        Default: the agent's name, or, for the builder's own agent,
                             the builder's command and args";

/// What a build may not read by default: the MergeLoop session, and where credentials live in
/// a home folder. The agent CLIs read their own login themselves, outside their sandboxes, so
/// denying it to what they run leaves them working.
pub const DENY_READ: &[&str] = &[
    "~/.mergeloop/session",
    "~/.ssh",
    "~/.aws",
    "~/.gnupg",
    "~/.netrc",
    "~/.git-credentials",
    "~/.config/gh",
    "~/.config/gcloud",
    "~/.azure",
    "~/.docker/config.json",
    "~/.kube",
    "~/.npmrc",
    "~/.cargo/credentials",
    "~/.cargo/credentials.toml",
    "~/.pypirc",
    "~/.gem/credentials",
    "~/.codex/auth.json",
    "~/.claude/.credentials.json",
];

/// Where the file is by default.
pub fn default_path(home: &Path) -> PathBuf {
    home.join(".mergeloop").join("daemon.toml")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessMode {
    Workspace,
    Full,
}

/// The daemon's settings, with the defaults filled in.
#[derive(Debug, Clone)]
pub struct Config {
    pub slots: usize,
    pub workspace: PathBuf,
    pub access: AccessMode,
    /// What a build may not read in `workspace` access.
    pub deny_read: Vec<PathBuf>,
    pub builder: LocalAgent,
    /// Reviews each build's change before it is pushed. `None` turns the review off.
    pub reviewer: Option<LocalAgent>,
}

impl Config {
    /// The access the agent runs with.
    pub fn access(&self) -> Access {
        match self.access {
            AccessMode::Workspace => Access::workspace(&self.deny_read),
            AccessMode::Full => Access::Full,
        }
    }

    /// The file at `path`, or the defaults when there is none. `home` is where `~` points.
    pub fn load(path: &Path, home: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
        };
        Self::parse(&text, home)
            .with_context(|| format!("{} is not a daemon config", path.display()))
    }

    pub fn parse(text: &str, home: &Path) -> Result<Self> {
        Self::read(text, home, &on_path)
    }

    /// `parse`, where `installed` says whether a program of that name is on `PATH`.
    fn read(text: &str, home: &Path, installed: &dyn Fn(&str) -> bool) -> Result<Self> {
        // toml's error names the line and column.
        let file: File = toml::from_str(text)?;
        if file.slots == 0 {
            bail!("slots must be at least 1");
        }
        let workspace = match file.workspace {
            Some(w) => expand(&w, home),
            None => home.join(".mergeloop").join("daemon"),
        };
        if !workspace.is_absolute() {
            bail!("workspace must be an absolute path, or start with ~/");
        }
        let deny_read: Vec<PathBuf> = match file.deny_read {
            Some(paths) => paths.iter().map(|p| expand(p, home)).collect(),
            None => DENY_READ.iter().map(|p| expand(p, home)).collect(),
        };
        for path in &deny_read {
            if !path.is_absolute() {
                bail!(
                    "deny_read paths must be absolute, or start with ~/: {}",
                    path.display()
                );
            }
            if workspace.starts_with(path) || path.starts_with(&workspace) {
                bail!(
                    "the workspace {} and the deny_read path {} overlap: a build could not read its own folder, or could read what it may not",
                    workspace.display(),
                    path.display()
                );
            }
        }
        let builder = file.builder.agent("builder", None)?;
        let reviewer = match file.reviewer.enabled {
            true => Some(file.reviewer.agent(&builder, installed)?),
            false => None,
        };
        Ok(Config {
            slots: file.slots,
            workspace,
            access: file.access,
            deny_read,
            builder,
            reviewer,
        })
    }
}

/// Whether a program named `name` is in a folder on `PATH`.
fn on_path(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|path| {
        std::env::split_paths(&path).any(|dir| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(dir.join(name))
                .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        })
    })
}

fn blank(v: Option<String>) -> Option<String> {
    v.filter(|v| !v.trim().is_empty())
}

impl AgentFile {
    /// The agent of section `section`. `command` is its default command and args, when not
    /// the agent's name and none.
    fn agent(self, section: &str, command: Option<(&str, &[String])>) -> Result<LocalAgent> {
        if self.timeout_minutes == 0 {
            bail!("{section}.timeout_minutes must be at least 1");
        }
        let (command, args) = match (blank(self.command), command) {
            (Some(c), _) => (c, self.args),
            (None, Some((c, args))) if self.args.is_empty() => (c.to_owned(), args.to_vec()),
            (None, _) => (self.agent.name().to_owned(), self.args),
        };
        Ok(LocalAgent {
            vendor: self.agent,
            command,
            model: blank(self.model),
            effort: blank(self.effort),
            args,
            timeout: Duration::from_secs(60 * self.timeout_minutes),
            max_turns: MAX_TURNS,
        })
    }
}

impl ReviewerFile {
    /// The reviewer: by default the other vendor's CLI when it is installed, a fresh look by
    /// another model; else the builder's own CLI.
    fn agent(self, builder: &LocalAgent, installed: &dyn Fn(&str) -> bool) -> Result<LocalAgent> {
        let other = match builder.vendor {
            Vendor::Claude => Vendor::Codex,
            Vendor::Codex => Vendor::Claude,
        };
        let vendor = match self.agent {
            Some(vendor) => vendor,
            None if self.command.is_none() && installed(other.name()) => other,
            None => builder.vendor,
        };
        let same =
            (vendor == builder.vendor).then_some((builder.command.as_str(), &builder.args[..]));
        AgentFile {
            agent: vendor,
            command: self.command,
            model: self.model,
            effort: self.effort,
            args: self.args,
            timeout_minutes: self.timeout_minutes,
        }
        .agent("reviewer", same)
    }
}

fn expand(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if path == "~" => home.to_owned(),
        None => PathBuf::from(path),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct File {
    slots: usize,
    workspace: Option<String>,
    access: AccessMode,
    deny_read: Option<Vec<String>>,
    builder: AgentFile,
    reviewer: ReviewerFile,
}

impl Default for File {
    fn default() -> Self {
        File {
            slots: 1,
            workspace: None,
            access: AccessMode::Workspace,
            deny_read: None,
            builder: AgentFile::default(),
            reviewer: ReviewerFile::default(),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct AgentFile {
    #[serde(with = "vendor")]
    agent: Vendor,
    command: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    args: Vec<String>,
    timeout_minutes: u64,
}

impl Default for AgentFile {
    fn default() -> Self {
        AgentFile {
            agent: Vendor::Claude,
            command: None,
            model: None,
            effort: None,
            args: vec![],
            timeout_minutes: 60,
        }
    }
}

/// `[reviewer]`: an `AgentFile` whose agent has no fixed default, and that can be turned off.
#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct ReviewerFile {
    enabled: bool,
    #[serde(default, with = "vendor::optional")]
    agent: Option<Vendor>,
    command: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    args: Vec<String>,
    timeout_minutes: u64,
}

impl Default for ReviewerFile {
    fn default() -> Self {
        let b = AgentFile::default();
        ReviewerFile {
            enabled: true,
            agent: None,
            command: b.command,
            model: b.model,
            effort: b.effort,
            args: b.args,
            timeout_minutes: b.timeout_minutes,
        }
    }
}

mod vendor {
    use mergepatrol_resolver::Vendor;
    use serde::{Deserialize, Deserializer, de::Error};

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vendor, D::Error> {
        match String::deserialize(d)?.as_str() {
            "claude" => Ok(Vendor::Claude),
            "codex" => Ok(Vendor::Codex),
            other => Err(D::Error::custom(format!(
                "unknown agent `{other}`, expected `claude` or `codex`"
            ))),
        }
    }

    pub mod optional {
        use mergepatrol_resolver::Vendor;
        use serde::Deserializer;

        pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Vendor>, D::Error> {
            super::deserialize(d).map(Some)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn home() -> PathBuf {
        PathBuf::from("/home/ana")
    }

    #[test]
    fn no_file_is_the_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let config = Config::load(&dir.path().join("daemon.toml"), &home()).unwrap();
        assert_eq!(config.slots, 1);
        assert_eq!(config.workspace, home().join(".mergeloop/daemon"));
        assert_eq!(config.access, AccessMode::Workspace);
        assert!(
            config
                .deny_read
                .contains(&home().join(".mergeloop/session"))
        );
        assert!(config.deny_read.contains(&home().join(".ssh")));
        assert_eq!(config.deny_read.len(), DENY_READ.len());
        assert_eq!(config.access(), Access::workspace(&config.deny_read));
        let b = &config.builder;
        assert_eq!((b.vendor, b.command.as_str()), (Vendor::Claude, "claude"));
        assert_eq!((b.model.as_deref(), b.effort.as_deref()), (None, None));
        assert_eq!(b.timeout, Duration::from_secs(3600));
    }

    #[test]
    fn every_setting_is_read() {
        let config = Config::parse(
            r#"
slots = 3
workspace = "~/builds"
access = "full"

[builder]
agent = "codex"
command = "my-codex"
model = "gpt-6-luna"
effort = "low"
args = ["--enable", "x"]
timeout_minutes = 5
"#,
            &home(),
        )
        .unwrap();
        assert_eq!(config.slots, 3);
        assert_eq!(config.workspace, home().join("builds"));
        assert_eq!(config.access(), Access::Full);
        let b = &config.builder;
        assert_eq!((b.vendor, b.command.as_str()), (Vendor::Codex, "my-codex"));
        assert_eq!(b.model.as_deref(), Some("gpt-6-luna"));
        assert_eq!(b.effort.as_deref(), Some("low"));
        assert_eq!(b.args, ["--enable", "x"]);
        assert_eq!(b.timeout, Duration::from_secs(300));
    }

    #[test]
    fn deny_read_replaces_the_list_and_may_not_overlap_the_workspace() {
        let config =
            Config::parse("deny_read = [\"~/.secrets\", \"/etc/token\"]\n", &home()).unwrap();
        assert_eq!(
            config.deny_read,
            [home().join(".secrets"), PathBuf::from("/etc/token")]
        );
        for text in [
            "workspace = \"~/.ssh/builds\"\n",
            "workspace = \"~/w\"\ndeny_read = [\"~/w/secret\"]\n",
            "deny_read = [\"relative\"]\n",
        ] {
            assert!(Config::parse(text, &home()).is_err(), "{text}");
        }
    }

    #[test]
    fn the_command_defaults_to_the_agents_name() {
        let config = Config::parse("[builder]\nagent = \"codex\"\n", &home()).unwrap();
        assert_eq!(config.builder.command, "codex");
        let config = Config::parse("[builder]\ncommand = \"mclaude\"\n", &home()).unwrap();
        assert_eq!(
            (config.builder.vendor, config.builder.command.as_str()),
            (Vendor::Claude, "mclaude")
        );
    }

    fn reviewer(text: &str, installed: &[&str]) -> Option<LocalAgent> {
        Config::read(text, &home(), &|name| installed.contains(&name))
            .unwrap()
            .reviewer
    }

    #[test]
    fn the_reviewer_is_the_other_agent_when_its_cli_is_installed_else_the_builders() {
        let r = reviewer("", &["claude", "codex"]).unwrap();
        assert_eq!((r.vendor, r.command.as_str()), (Vendor::Codex, "codex"));
        let codex = "[builder]\nagent = \"codex\"\n";
        let r = reviewer(codex, &["claude", "codex"]).unwrap();
        assert_eq!((r.vendor, r.command.as_str()), (Vendor::Claude, "claude"));

        let mclaude = "[builder]\ncommand = \"mclaude\"\nargs = [\"--strict-mcp-config\"]\n";
        let r = reviewer(mclaude, &["mclaude"]).unwrap();
        assert_eq!((r.vendor, r.command.as_str()), (Vendor::Claude, "mclaude"));
        assert_eq!(r.args, ["--strict-mcp-config"]);
        assert_eq!(r.model, None);
    }

    #[test]
    fn the_reviewer_section_names_its_agent_or_turns_it_off() {
        let text = "[builder]\ncommand = \"mclaude\"\n\n[reviewer]\ncommand = \"mclaude\"\nmodel = \"haiku\"\neffort = \"low\"\nargs = [\"-x\"]\ntimeout_minutes = 5\n";
        let r = reviewer(text, &["codex"]).unwrap();
        assert_eq!((r.vendor, r.command.as_str()), (Vendor::Claude, "mclaude"));
        assert_eq!(
            (r.model.as_deref(), r.effort.as_deref()),
            (Some("haiku"), Some("low"))
        );
        assert_eq!(
            (r.args.as_slice(), r.timeout),
            (&["-x".to_owned()][..], Duration::from_secs(300))
        );
        let r = reviewer("[reviewer]\nagent = \"codex\"\n", &[]).unwrap();
        assert_eq!((r.vendor, r.command.as_str()), (Vendor::Codex, "codex"));
        assert!(reviewer("[reviewer]\nenabled = false\n", &["codex"]).is_none());
        for bad in [
            "[reviewer]\ntimeout_minutes = 0\n",
            "[reviewer]\nagent = \"x\"\n",
            "[reviewer]\nmodle = \"x\"\n",
        ] {
            assert!(Config::read(bad, &home(), &|_| true).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_bad_file_is_an_error_that_names_the_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.toml");
        for (text, says) in [
            ("slots = 2\naccess = \"wide\"\n", "line 2"),
            ("slots = 2\n\n[builder]\nagnet = \"claude\"\n", "line 4"),
            ("[builder]\nagent = \"gemini\"\n", "line 2"),
            ("slots = \n", "line 1"),
        ] {
            std::fs::write(&path, text).unwrap();
            let err = format!("{:#}", Config::load(&path, &home()).unwrap_err());
            assert!(err.contains(says), "{text:?}: {err}");
            assert!(err.contains("daemon.toml"), "{err}");
        }
        assert!(Config::parse("slots = 0", &home()).is_err());
        assert!(Config::parse("workspace = \"rel/dir\"", &home()).is_err());
    }
}
