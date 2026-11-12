//! GitHub, as a GitHub App: auth, the queue snapshot, the Git Data API, checks, and merges.

mod app;
mod checks;
mod codeowners;
mod files;
mod issues;
mod repo;
pub mod setup;

#[cfg(any(test, feature = "testing"))]
pub use app::fake;
pub use app::{
    App, AsUser, GitHub, HookDelivery, Installation, InstallationInfo, RateLimited, RepoInfo,
    ScopedToken, revoke_token,
};
pub use checks::{BranchRules, CheckState, Checks};
pub use codeowners::CodeOwners;
pub use files::{BranchFile, ConfigFile, GitObject, ObjectsAt, Proposal, PullRef, SYMLINK_MODE};
pub use issues::Issue;
pub use repo::{
    BranchMerge, BranchMove, HeadRepository, MergeOutcome, PrDetails, PullRequest, RepoOwner,
    RepoSnapshot, ReviewThread, SnapshotParts, StackRef, ThreadComment, TreeEntry, queued,
};
pub use setup::{AppCredentials, GithubUser};

/// `s` as one part of a URL: a path segment or a query value.
pub fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}
