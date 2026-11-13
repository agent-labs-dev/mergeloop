//! The daemon's workspace folder. It never touches the developer's own checkouts:
//!
//! ```text
//! <workspace>/
//!   daemon/lock               held while a daemon uses the folder: one at a time
//!   daemon/id                 the daemon's name for itself, made once: its claims are its own
//!   daemon/groups/            the process groups of running builds (`groups`)
//!   repos/<owner>/<name>.git  a bare clone of each repo it builds in, with its worktrees' git dirs
//!   builds/<build>/           the build's room: all that the build may write
//!     tree/                   its git worktree, on the local branch `mergeloop/build-<issue>`
//!     tmp/                    its TMPDIR
//!   reviews/<build>/          the reviewer's room, made empty for each review: all that the
//!     tmp/                    reviewer may write, and its TMPDIR. It reads the build's tree
//!   transcripts/<build>/      the agents' output, kept 7 days
//! ```
//!
//! Nothing of the daemon's, nor any git metadata or configuration, is in a room: what a build
//! runs cannot change what the daemon runs next. The daemon's git reads a build's tree only while
//! its root is the folder made (`Tree`), and in `workspace` access under the verify commands'
//! confinement (`verify::confine_tree`), so a tree pointed elsewhere gives it nothing denied to
//! read. A build's room and branch go when the build ends, however it
//! ends. Every removal is idempotent, and `reconcile` on start removes whatever a crash left of
//! builds the daemon does not hold.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use mergepatrol_git::{Git, Tree};
use mergepatrol_resolver::{Access, Room};

use super::groups::Groups;
use super::lane::Task;
use super::verify;

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
        let open = || {
            std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(own.join("lock"))
        };
        // Something in its place that is no file is no lock anyone holds.
        let lock = open().or_else(|e| {
            discard(&own.join("lock"), Some(&e.into()));
            open()
        })?;
        // A child forked in this process shares the lock until it execs: give it a moment.
        let locked = (0..50).any(|_| {
            lock.try_lock().is_ok() || {
                std::thread::sleep(std::time::Duration::from_millis(20));
                false
            }
        });
        if !locked {
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

    /// The process groups the builds run.
    pub fn groups(&self) -> Groups {
        Groups::new(self.root.join("daemon").join("groups"))
    }

    pub fn clone_of(&self, repo: &str) -> PathBuf {
        self.root.join("repos").join(format!("{repo}.git"))
    }

    /// The build's room: its tree, and its temp folder.
    pub fn room(&self, build: u64) -> Room {
        let root = self.root.join("builds").join(build.to_string());
        Room {
            tree: root.join("tree"),
            root,
        }
    }

    /// A reviewer's room, made empty: its own folder, holding its temp folder, and the one place
    /// it may write. It reads the build's tree from there. It is outside the build's room, so
    /// nothing the builder wrote is in a folder around it, where a CLI looks for instructions.
    pub async fn review_room(&self, build: u64) -> Result<Room> {
        let root = self.reviews(build);
        remove_dir(&root).await?;
        tokio::fs::create_dir_all(&root).await?;
        Ok(Room {
            tree: root.clone(),
            root,
        })
    }

    fn reviews(&self, build: u64) -> PathBuf {
        self.root.join("reviews").join(build.to_string())
    }

    pub fn transcripts(&self, build: u64) -> PathBuf {
        self.root.join("transcripts").join(build.to_string())
    }

    fn clone_lock(&self, clone: &Path) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.clones.lock().unwrap();
        locks.entry(clone.to_owned()).or_default().clone()
    }

    /// A fresh room for the task, its tree on the task's branch at the default branch as GitHub
    /// has it now: fetched with `token`. The daemon's git on the tree is confined to `access`.
    pub async fn checkout(&self, task: &Task, token: &str, access: &Access) -> Result<Checkout> {
        let clone = self.clone_of(&task.repo);
        let lock = self.clone_lock(&clone);
        let _held = lock.lock().await;
        self.clone_at(&clone, &task.clone_url).await?;
        let base = format!("refs/remotes/origin/{}", task.default_branch);
        let refspec = format!("+refs/heads/{}:{base}", task.default_branch);
        Git.fetch(&clone, token, &[&refspec]).await?;
        let room = self.room(task.build);
        self.remove_locked(&clone, &room.root, &task.branch).await?;
        tokio::fs::create_dir_all(room.tmp()).await?;
        let tree = Git
            .add_tree(&clone, &room.tree, &task.branch, &base)
            .await?;
        let tree = verify::confine_tree(tree, &clone, access)?;
        let remote = Git.remote_head(&clone, token, &task.branch).await?;
        Ok(Checkout {
            clone,
            room,
            tree,
            remote,
        })
    }

    /// A fresh room for the task's PR, its tree at the PR's branch as GitHub has it now: fetched
    /// with `token`. A push replaces only that head. The daemon's git on the tree is confined to
    /// `access`.
    pub async fn checkout_pr(&self, task: &Task, token: &str, access: &Access) -> Result<Checkout> {
        let clone = self.clone_of(&task.repo);
        let lock = self.clone_lock(&clone);
        let _held = lock.lock().await;
        self.clone_at(&clone, &task.clone_url).await?;
        let pushed = format!("refs/remotes/origin/{}", task.branch);
        Git.fetch(
            &clone,
            token,
            &[&format!("+refs/heads/{}:{pushed}", task.branch)],
        )
        .await?;
        let head = Git.run(&clone, &["rev-parse", &pushed]).await?;
        let room = self.room(task.build);
        self.remove_locked(&clone, &room.root, &task.branch).await?;
        tokio::fs::create_dir_all(room.tmp()).await?;
        let tree = Git
            .add_tree(&clone, &room.tree, &task.branch, &pushed)
            .await?;
        let tree = verify::confine_tree(tree, &clone, access)?;
        Ok(Checkout {
            clone,
            room,
            tree,
            remote: Some(head.trim().to_owned()),
        })
    }

    /// Removes the build's room and its local branch. Nothing there is fine.
    pub async fn remove(&self, task: &Task) -> Result<()> {
        let clone = self.clone_of(&task.repo);
        let lock = self.clone_lock(&clone);
        let _held = lock.lock().await;
        remove_dir(&self.reviews(task.build)).await?;
        self.remove_locked(&clone, &self.room(task.build).root, &task.branch)
            .await
    }

    async fn remove_locked(&self, clone: &Path, room: &Path, branch: &str) -> Result<()> {
        remove_dir(room).await?;
        if !clone.join("HEAD").exists() {
            return Ok(());
        }
        Git.run(clone, &["worktree", "prune"]).await?;
        let refname = format!("refs/heads/{branch}");
        if Git
            .run(clone, &["show-ref", "--verify", "--quiet", &refname])
            .await
            .is_ok()
        {
            Git.run(clone, &["update-ref", "-d", &refname]).await?;
        }
        Ok(())
    }

    /// Removes every build's worktree and branch but those of `keep`, with whatever a crash
    /// left half made, and the transcripts older than `TRANSCRIPTS_KEPT`. Heals each clone a
    /// killed git left locked. What it cannot make sense of it removes, saying so: nothing left
    /// in the folder stops the daemon. Run it before any build starts.
    pub async fn reconcile(&self, keep: &[u64]) {
        let rooms = entries(&self.root.join("builds"));
        for dir in rooms.into_iter().chain(entries(&self.root.join("reviews"))) {
            let held = dir
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.parse::<u64>().ok())
                .is_some_and(|b| keep.contains(&b));
            if !held {
                discard(&dir, None);
            }
        }
        for owner in entries(&self.root.join("repos")) {
            for clone in entries(&owner) {
                if let Err(e) = self.reconcile_clone(&clone).await {
                    // Made again when a build next needs it.
                    discard(&clone, Some(&e));
                }
            }
        }
        if let Err(e) = self.prune_transcripts(SystemTime::now()).await {
            discard(&self.root.join("transcripts"), Some(&e));
        }
    }

    /// Makes `clone` a clone of `url`: made, or made whole when a crash left it before it had its
    /// origin, or pointed at `url` when the repo was renamed. Run it under the clone's lock.
    async fn clone_at(&self, clone: &Path, url: &str) -> Result<()> {
        match Git.run(clone, &["remote", "get-url", "origin"]).await {
            Ok(current) if current.trim() == url => Ok(()),
            Ok(_) => Git
                .run(clone, &["remote", "set-url", "origin", url])
                .await
                .map(drop),
            Err(_) => Git.ensure_cache(clone, url).await,
        }
    }

    async fn reconcile_clone(&self, clone: &Path) -> Result<()> {
        // Whole only with its origin: a clone a crash left before then is made again.
        let url = Git.run(clone, &["remote", "get-url", "origin"]).await?;
        Git.ensure_cache(clone, url.trim()).await?;
        Git.run(clone, &["worktree", "prune"]).await?;
        let used = Git.run(clone, &["worktree", "list", "--porcelain"]).await?;
        let used: Vec<&str> = used
            .lines()
            .filter_map(|l| l.strip_prefix("branch "))
            .collect();
        let branches = Git
            .run(
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
            Git.run(clone, &["update-ref", "-d", branch]).await?;
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

/// A build's checkout: its room, and its tree as git knows it, in its clone.
pub struct Checkout {
    pub clone: PathBuf,
    pub room: Room,
    pub tree: Tree,
    /// Where the task's branch was on GitHub at checkout: the push replaces only that.
    pub remote: Option<String>,
}

/// The name kept in `dir/id`, made the first time, or when what is there cannot be read.
/// Written whole or not at all.
fn identity(dir: &Path) -> Result<String> {
    let path = dir.join("id");
    match std::fs::read_to_string(&path) {
        Ok(id) if !id.trim().is_empty() => return Ok(id.trim().to_owned()),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        // A name it cannot read names no claim it could still act on.
        Err(e) => discard(&path, Some(&e.into())),
    }
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("the OS has no random source");
    let id: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let tmp = dir.join("id.tmp");
    std::fs::write(&tmp, format!("{id}\n"))?;
    std::fs::rename(&tmp, &path)?;
    Ok(id)
}

/// Removes `path`, a folder or not, saying why when `why` is given. Saying so is all it does
/// when that fails.
fn discard(path: &Path, why: Option<&anyhow::Error>) {
    if let Some(e) = why {
        eprintln!("mergeloop: removes {}: {e:#}", path.display());
    }
    let removed = std::fs::remove_dir_all(path).or_else(|_| std::fs::remove_file(path));
    match removed {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            eprintln!("mergeloop: could not remove {}: {e}", path.display());
        }
        _ => {}
    }
}

/// The entries of `dir`, none when it is not there. A `dir` that cannot be read is removed.
fn entries(dir: &Path) -> Vec<PathBuf> {
    list(dir).unwrap_or_else(|e| {
        discard(dir, Some(&e));
        vec![]
    })
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
