//! The sessions `mergeloop login` keeps: one per server, in a file only its owner may read. A
//! session is a credential, so it is never printed, logged, or put in a process's arguments.

use std::collections::BTreeMap;
use std::fmt;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

use crate::client::Server;

/// A token that must not be shown: its `Debug` says only that it is there.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(token: String) -> Self {
        Secret(token)
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(..)")
    }
}

/// Where the sessions are kept: `~/.mergeloop/session.json`.
pub fn path() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .context("no HOME to keep the session in")?;
    Ok(PathBuf::from(home).join(".mergeloop").join("session.json"))
}

/// The server's session, if one was kept.
pub fn find(path: &Path, server: &Server) -> Result<Option<Secret>> {
    Ok(read(path)?.remove(server.url()).map(Secret))
}

/// Keeps the server's session, in place of one it had.
pub fn keep(path: &Path, server: &Server, session: &Secret) -> Result<()> {
    change(path, |sessions| {
        sessions.insert(server.url().to_owned(), session.0.clone());
    })
}

/// Forgets the server's session, and returns it.
pub fn forget(path: &Path, server: &Server) -> Result<Option<Secret>> {
    Ok(change(path, |sessions| sessions.remove(server.url()))?.map(Secret))
}

/// Reads the sessions, changes them, and writes them back, while holding the folder's lock:
/// two commands that change the file at once each see the other's change.
fn change<T>(path: &Path, f: impl FnOnce(&mut BTreeMap<String, String>) -> T) -> Result<T> {
    let dir = path.parent().context("the session file has no folder")?;
    private_dir(dir)?;
    let lock = dir.join(".session.lock");
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock)
        .and_then(|file| file.lock().map(|()| file))
        .with_context(|| format!("lock {}", lock.display()))?;
    let mut sessions = read(path)?;
    let before = sessions.clone();
    let out = f(&mut sessions);
    if sessions != before {
        write(path, &sessions)?;
    }
    drop(lock);
    Ok(out)
}

/// Each server's URL and its session. No file is no sessions.
fn read(path: &Path) -> Result<BTreeMap<String, String>> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", path.display())),
    };
    // serde's error says where, never what: the file's text stays out of the message.
    serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not what `mergeloop login` writes: delete it and log in again",
            path.display()
        )
    })
}

/// Writes the file whole, readable by its owner only, in a folder only they may enter: a new
/// file, named for this write alone, renamed over the old, so a crash leaves one or the other.
fn write(path: &Path, sessions: &BTreeMap<String, String>) -> Result<()> {
    static WRITES: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().context("the session file has no folder")?;
    let n = WRITES.fetch_add(1, Ordering::Relaxed);
    let tmp = dir.join(format!(".session-{}-{n}.tmp", std::process::id()));
    let mut file = private_file(&tmp)?;
    let written = file
        .write_all(serde_json::to_string_pretty(sessions)?.as_bytes())
        .and_then(|()| file.sync_all())
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.with_context(|| format!("write {}", path.display()))
}

#[cfg(unix)]
fn private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .and_then(|()| std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)))
        .with_context(|| format!("make {}", dir.display()))
}

#[cfg(not(unix))]
fn private_dir(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))
}

#[cfg(unix)]
fn private_file(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("create {}", path.display()))
}

#[cfg(not(unix))]
fn private_file(path: &Path) -> Result<std::fs::File> {
    let _ = std::fs::remove_file(path);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .with_context(|| format!("create {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_is_not_shown_by_debug() {
        let shown = format!("{:?}", Secret::new("sess-123".into()));
        assert!(!shown.contains("sess-123"), "{shown}");
    }

    #[test]
    fn a_bad_file_is_named_without_its_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.json");
        std::fs::write(&path, "sess-123 not json").unwrap();
        let err = find(&path, &Server::parse("http://x").unwrap()).unwrap_err();
        let said = format!("{err:#}");
        assert!(
            said.contains("session.json") && !said.contains("sess-123"),
            "{said}"
        );
    }

    #[test]
    fn each_server_keeps_its_own_session() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m").join("session.json");
        let (a, b) = (
            Server::parse("http://a").unwrap(),
            Server::parse("http://b").unwrap(),
        );
        assert!(find(&path, &a).unwrap().is_none());
        keep(&path, &a, &Secret::new("sa".into())).unwrap();
        keep(&path, &b, &Secret::new("sb".into())).unwrap();
        assert_eq!(forget(&path, &a).unwrap().unwrap().expose(), "sa");
        assert!(find(&path, &a).unwrap().is_none());
        assert_eq!(find(&path, &b).unwrap().unwrap().expose(), "sb");
        // Only the file and its lock are left in the folder: no temporary file.
        let mut left: Vec<String> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, [".session.lock", "session.json"]);
    }

    #[test]
    fn sessions_kept_and_forgotten_at_once_lose_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m").join("session.json");
        let server = |i: usize| Server::parse(&format!("http://s{i}")).unwrap();
        keep(&path, &server(0), &Secret::new("gone".into())).unwrap();
        std::thread::scope(|s| {
            for i in 1..=16 {
                let path = &path;
                s.spawn(move || keep(path, &server(i), &Secret::new(format!("s{i}"))).unwrap());
            }
            s.spawn(|| forget(&path, &server(0)).unwrap());
        });
        assert!(find(&path, &server(0)).unwrap().is_none());
        for i in 1..=16 {
            let kept = find(&path, &server(i)).unwrap();
            assert_eq!(kept.map(|s| s.0), Some(format!("s{i}")), "server {i}");
        }
    }
}
