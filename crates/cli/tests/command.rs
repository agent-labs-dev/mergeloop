//! The `mergeloop` binary, run as a user runs it, against a fake server: which credential it
//! sends, and what it says when the server refuses it.

mod fake;

use std::path::Path;
use std::process::Output;

use mergeloop_cli::client::Server;
use mergeloop_cli::session;
use mergeloop_cli::session::Secret;

/// Runs `mergeloop` with `home` as `$HOME`, against the fake, with a `gh` that prints the
/// fake's GitHub token, and `MERGELOOP_TOKEN` only when given.
async fn mergeloop(fake: &fake::Server, home: &Path, token: Option<&str>, args: &[&str]) -> Output {
    let bin = home.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let gh = bin.join("gh");
    std::fs::write(&gh, format!("#!/bin/sh\necho {}\n", fake::GITHUB_TOKEN)).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_mergeloop"));
    cmd.args(args)
        .env("HOME", home)
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env("MERGELOOP_URL", &fake.url)
        .env_remove("MERGELOOP_TOKEN");
    if let Some(token) = token {
        cmd.env("MERGELOOP_TOKEN", token);
    }
    cmd.output().await.unwrap()
}

fn sign_in(fake: &fake::Server, home: &Path, session: &str) {
    fake.add_session(session);
    let path = home.join(".mergeloop").join("session.json");
    session::keep(
        &path,
        &Server::parse(&fake.url).unwrap(),
        &Secret::new(session.to_owned()),
    )
    .unwrap();
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_session_a_call_sends_the_gh_token() {
    let fake = fake::Server::start().await;
    let home = tempfile::tempdir().unwrap();

    let out = mergeloop(&fake, home.path(), None, &["call", "pull_status", "{}"]).await;

    assert!(out.status.success(), "{out:?}");
    assert!(stdout(&out).contains(r#""as":"github""#), "{out:?}");
    assert_eq!(fake.bearers(), [fake::GITHUB_TOKEN]);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_a_session_a_pr_is_queued_and_its_need_printed_with_the_gh_token() {
    let fake = fake::Server::start().await;
    let home = tempfile::tempdir().unwrap();

    let out = mergeloop(&fake, home.path(), None, &["o/r#5"]).await;

    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let need: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(need["needs"], "threads");
    assert_eq!(need["then"], "mergeloop o/r#5 --after t1");
    assert_eq!(fake.tools(), ["next_need", "queue_pull", "next_need"]);
    assert!(fake.bearers().iter().all(|b| b == fake::GITHUB_TOKEN));
}

#[tokio::test(flavor = "multi_thread")]
async fn signed_in_a_call_sends_the_session() {
    let fake = fake::Server::start().await;
    let home = tempfile::tempdir().unwrap();
    sign_in(&fake, home.path(), "sess");

    let out = mergeloop(&fake, home.path(), None, &["call", "list_repos"]).await;

    assert!(out.status.success(), "{out:?}");
    assert!(stdout(&out).contains(r#""as":"session""#), "{out:?}");
    assert_eq!(fake.bearers(), ["sess"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn mergeloop_token_wins_over_a_session() {
    let fake = fake::Server::start().await;
    let home = tempfile::tempdir().unwrap();
    sign_in(&fake, home.path(), "sess");

    let out = mergeloop(
        &fake,
        home.path(),
        Some(fake::GITHUB_TOKEN),
        &["call", "pull_status"],
    )
    .await;

    assert!(out.status.success(), "{out:?}");
    assert_eq!(fake.bearers(), [fake::GITHUB_TOKEN]);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rejected_session_says_to_log_in_and_exits_non_zero() {
    let fake = fake::Server::start().await;
    let home = tempfile::tempdir().unwrap();
    sign_in(&fake, home.path(), "sess");
    fake.seen.lock().unwrap().sessions.clear();

    let out = mergeloop(&fake, home.path(), None, &["call", "list_repos"]).await;

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(stderr.contains("run `mergeloop login`"), "{stderr}");
    assert!(
        !stderr.contains("sess\n") && !stderr.contains(" sess"),
        "{stderr}"
    );
    assert_eq!(fake.bearers().len(), 1, "no retry");
}

#[tokio::test(flavor = "multi_thread")]
async fn logout_without_a_session_says_so() {
    let fake = fake::Server::start().await;
    let home = tempfile::tempdir().unwrap();

    let out = mergeloop(&fake, home.path(), None, &["logout"]).await;

    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not signed in"),
        "{out:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn help_says_what_is_sent_and_where_the_session_is_kept() {
    let fake = fake::Server::start().await;
    let home = tempfile::tempdir().unwrap();

    let out = mergeloop(&fake, home.path(), None, &["--help"]).await;

    let help = stdout(&out);
    assert!(out.status.success(), "{out:?}");
    for said in [
        "mergeloop login",
        "mergeloop logout",
        "~/.mergeloop/session.json",
        "MERGELOOP_TOKEN",
    ] {
        assert!(help.contains(said), "{said}: {help}");
    }
    assert!(fake.bearers().is_empty(), "help calls nobody");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_call_given_mergeloop_token_needs_no_home() {
    let fake = fake::Server::start().await;
    let home = tempfile::tempdir().unwrap();
    let mut cmd = tokio::process::Command::new(env!("CARGO_BIN_EXE_mergeloop"));
    cmd.args(["call", "pull_status", "{}"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("MERGELOOP_URL", &fake.url)
        .env("MERGELOOP_TOKEN", fake::GITHUB_TOKEN)
        .current_dir(home.path());

    let out = cmd.output().await.unwrap();

    assert!(out.status.success(), "{out:?}");
    assert_eq!(fake.bearers(), [fake::GITHUB_TOKEN]);
}
