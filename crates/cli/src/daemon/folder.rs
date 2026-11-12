//! The daemon's workspace folder. It never touches the developer's own checkouts:
//!
//! ```text
//! <workspace>/
//!   daemon/lock               held while a daemon uses the folder: one at a time
//!   daemon/id                 the daemon's name for itself, made once: its claims are its own
//!   repos/<owner>/<name>.git  a bare clone of each repo it builds in
//!   builds/<build>/           a git worktree per build, on the local branch `mergeloop/build-<issue>`
//!   transcripts/<build>/      the agents' output, kept 7 days
//!   tmp/                      the agents' TMPDIR
//! ```
//!
//! A build's worktree and branch go when the build ends, however it ends. Every removal is
//! idempotent, and `reconcile` on start removes whatever a crash left of builds the daemon does
//! not hold.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use mergepatrol_git::Git;

use super::lane::{Identity, Task};

/// How long transcripts are kept.
pub const TRANSCRIPTS_KEPT: Duration = Duration::from_secs(7 * 24 * 3600);
/// The local branches of builds, in every clone.
const BRANCHES: &str = "refs/heads/mergeloop/build-";

/// The workspace folder, and a lock per clone: git commands on one clone run one at a time.
#[derive(Clone)]
pub struct Folder {
    root: PathBuf,
    /// The daemon's name for itself, kept in the folder.
    id: String,
    clones: Arc<Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>>,
}

impl Folder {
    /// The folder at `root`, made if it is not there, and held for this process: an error when
    /// another daemon holds it. The lock goes with the returned file.
    pub fn open(root: &Path) -> Result<(Self, std::fs::File)> {
        std::fs::create_dir_all(root).with_context(|| format!("make {}", root.display()))?;
        // The agents' sandboxes see the folder by its real path (`/tmp` is `/private/tmp` on
        // macOS).
        let root = root.canonicalize()?;
        let own = root.join("daemon");
        std::fs::create_dir_all(&own)?;
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(own.join("lock"))?;
        if lock.try_lock().is_err() {
            bail!(
                "another mergeloop daemon is using {}: stop it, or give this one another workspace",
                root.display()
            );
        }
        let folder = Folder {
            id: identity(&own)?,
            root,
            clones: Arc::default(),
        };
        Ok((folder, lock))
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The daemon's name for itself, which its claims carry: it holds only the builds it
    /// claimed, and another daemon, of the same login too, holds none of them.
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn tmp(&self) -> PathBuf {
        self.root.join("tmp")
    }

    pub fn clone_of(&self, repo: &str) -> PathBuf {
        self.root.join("repos").join(format!("{repo}.git"))
    }

    pub fn worktree(&self, build: u64) -> PathBuf {
        self.root.join("builds").join(build.to_string())
    }

    pub fn transcripts(&self, build: u64) -> PathBuf {
        self.root.join("transcripts").join(build.to_string())
    }

    fn clone_lock(&self, clone: &Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.clones.lock().unwrap();
        locks.entry(clone.to_owned()).or_default().clone()
    }

    /// A fresh worktree for the task on its branch, at the default branch as GitHub has it now:
    /// fetched with `token`.
    pub async fn checkout(&self, task: &Task, token: &str) -> Result<PathBuf> {
        let clone = self.clone_of(&task.repo);
        let lock = self.clone_lock(&clone);
        let _held = lock.lock().await;
        if !clone.join("HEAD").exists() {
            Git.ensure_cache(&clone, &task.clone_url).await?;
        }
        let base = format!("refs/remotes/origin/{}", task.default_branch);
        let refspec = format!("+refs/heads/{}:{base}", task.default_branch);
        Git.fetch(&clone, token, &[&refspec]).await?;
        let dir = self.worktree(task.build);
        self.remove_locked(&clone, &dir, &task.branch).await?;
        tokio::fs::create_dir_all(self.root.join("builds")).await?;
        git(
            &clone,
            &[
                "worktree",
                "add",
                "-q",
                "-B",
                &task.branch,
                path(&dir)?,
                &base,
            ],
        )
        .await?;
        Ok(dir)
    }

    /// Removes the build's worktree and its local branch. Nothing there is fine.
    pub async fn remove(&self, task: &Task) -> Result<()> {
        let clone = self.clone_of(&task.repo);
        let lock = self.clone_lock(&clone);
        let _held = lock.lock().await;
        self.remove_locked(&clone, &self.worktree(task.build), &task.branch)
            .await
    }

    async fn remove_locked(&self, clone: &Path, dir: &Path, branch: &str) -> Result<()> {
        remove_dir(dir).await?;
        if !clone.join("HEAD").exists() {
            return Ok(());
        }
        git(clone, &["worktree", "prune"]).await?;
        let refname = format!("refs/heads/{branch}");
        if git(clone, &["show-ref", "--verify", "--quiet", &refname])
            .await
            .is_ok()
        {
            git(clone, &["update-ref", "-d", &refname]).await?;
        }
        Ok(())
    }

    /// Removes every build's worktree and branch but those of `keep`, with whatever a crash
    /// left half made, and the transcripts older than `TRANSCRIPTS_KEPT`. Heals each clone a
    /// killed git left locked. Run it before any build starts.
    pub async fn reconcile(&self, keep: &[u64]) -> Result<()> {
        for dir in list(&self.root.join("builds"))? {
            let held = dir
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.parse::<u64>().ok())
                .is_some_and(|b| keep.contains(&b));
            if !held {
                remove_dir(&dir).await?;
            }
        }
        for owner in list(&self.root.join("repos"))? {
            for clone in list(&owner)? {
                self.reconcile_clone(&clone).await?;
            }
        }
        self.prune_transcripts(SystemTime::now()).await
    }

    async fn reconcile_clone(&self, clone: &Path) -> Result<()> {
        if !clone.join("HEAD").exists() {
            // A clone a crash left before `git init`: nothing in it to keep.
            return remove_dir(clone).await;
        }
        let url = git(clone, &["remote", "get-url", "origin"]).await?;
        Git.ensure_cache(clone, url.trim()).await?;
        git(clone, &["worktree", "prune"]).await?;
        let used = git(clone, &["worktree", "list", "--porcelain"]).await?;
        let used: Vec<&str> = used
            .lines()
            .filter_map(|l| l.strip_prefix("branch "))
            .collect();
        let branches = git(
            clone,
            &[
                "for-each-ref",
                "--format=%(refname)",
                "refs/heads/mergeloop/",
            ],
        )
        .await?;
        let stale = branches
            .lines()
            .filter(|b| b.starts_with(BRANCHES) && !used.contains(b));
        for branch in stale {
            git(clone, &["update-ref", "-d", branch]).await?;
        }
        Ok(())
    }

    /// Removes the transcripts of builds last written to more than `TRANSCRIPTS_KEPT` before
    /// `now`.
    pub async fn prune_transcripts(&self, now: SystemTime) -> Result<()> {
        for dir in list(&self.root.join("transcripts"))? {
            let written = std::fs::metadata(&dir)?.modified()?;
            if now.duration_since(written).unwrap_or_default() > TRANSCRIPTS_KEPT {
                remove_dir(&dir).await?;
            }
        }
        Ok(())
    }
}

/// Commits everything in `wt` since `base` as one commit by `author`, with `co_author` in a
/// trailer. The agent may have left its change uncommitted, or committed it: either way it is
/// one commit. `false` when there is nothing to commit.
pub async fn commit(
    wt: &Path,
    base: &str,
    subject: &str,
    author: &Identity,
    co_author: &Identity,
) -> Result<bool> {
    git(wt, &["reset", "-q", "--soft", base]).await?;
    git(wt, &["add", "-A"]).await?;
    if git(wt, &["diff", "--cached", "--quiet"]).await.is_ok() {
        return Ok(false);
    }
    let message = format!(
        "{subject}\n\nCo-authored-by: {} <{}>\n",
        co_author.name, co_author.email
    );
    let mut cmd = command(
        wt,
        &[
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "--no-verify",
            "-F",
            "-",
        ],
    );
    cmd.env("GIT_AUTHOR_NAME", &author.name)
        .env("GIT_AUTHOR_EMAIL", &author.email)
        .env("GIT_COMMITTER_NAME", &author.name)
        .env("GIT_COMMITTER_EMAIL", &author.email)
        .stdin(Stdio::piped());
    let mut child = cmd.spawn().context("git commit: spawn")?;
    use tokio::io::AsyncWriteExt;
    let mut stdin = child.stdin.take().context("stdin")?;
    stdin.write_all(message.as_bytes()).await?;
    drop(stdin);
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!(
            "git commit: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(true)
}

/// The commit at `rev` in `wt`.
pub async fn rev_parse(wt: &Path, rev: &str) -> Result<String> {
    Ok(git(wt, &["rev-parse", "--verify", rev])
        .await?
        .trim()
        .to_owned())
}

/// The name kept in `dir/id`, made the first time. Written whole or not at all.
fn identity(dir: &Path) -> Result<String> {
    let path = dir.join("id");
    match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => return Ok(id.trim().to_owned()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS has no random source");
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let tmp = dir.join("id.tmp");
    std::fs::write(&tmp, format!("{id}\n"))?;
    std::fs::rename(&tmp, &path)?;
    Ok(id)
}

/// The folder's entries, none when it is not there.
fn list(dir: &Path) -> Result<Vec<PathBuf>> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|e| Ok(e?.path()))
            .collect::<std::io::Result<_>>()
            .with_context(|| format!("read {}", dir.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e).with_context(|| format!("read {}", dir.display())),
    }
}

async fn remove_dir(dir: &Path) -> Result<()> {
    match tokio::fs::remove_dir_all(dir).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| format!("remove {}", dir.display())),
    }
}

fn path(p: &Path) -> Result<&str> {
    p.to_str().context("a path that is not UTF-8")
}

fn command(cwd: &Path, args: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("git");
    cmd.current_dir(cwd)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

async fn git(cwd: &Path, args: &[&str]) -> Result<String> {
    let out = command(cwd, args)
        .output()
        .await
        .with_context(|| format!("git {}: spawn", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8(out.stdout)?)
}
