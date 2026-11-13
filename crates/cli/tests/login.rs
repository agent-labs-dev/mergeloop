//! `mergeloop login` and `logout` against a fake server, with a request to the sign-in link in
//! place of the browser.

mod fake;

use std::path::{Path, PathBuf};

use mergeloop_cli::client::{Client, Server};
use mergeloop_cli::login;
use serde_json::json;

/// The browser: follows the sign-in link to the consent page, checks it shows the code the link
/// carries, and answers `decision`, which sends it on to the command's listener.
fn answering(decision: &'static str) -> impl FnOnce(&str) -> anyhow::Result<()> {
    move |url: &str| {
        let url = reqwest::Url::parse(url).unwrap();
        tokio::spawn(async move {
            let code = url
                .query_pairs()
                .find(|(k, _)| k == "user_code")
                .map(|(_, v)| v.into_owned())
                .expect("the link carries a code");
            let page = reqwest::get(url.clone())
                .await
                .unwrap()
                .text()
                .await
                .unwrap();
            assert!(page.contains(&code), "{page}");
            assert!(page.contains("mergeloop login"), "{page}");
            let request = page
                .split(r#"name="request" value=""#)
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .expect("the page carries its request");
            let answer = url.join("/oauth/consent").unwrap();
            let res = reqwest::Client::new()
                .post(answer)
                .form(&[("request", request), ("decision", decision)])
                .send()
                .await
                .expect("the answer reaches the listener");
            assert!(res.status().is_success(), "{}", res.status());
        });
        Ok(())
    }
}

/// The user allows the sign-in.
fn browser(url: &str) -> anyhow::Result<()> {
    answering("allow")(url)
}

fn sessions(home: &Path) -> PathBuf {
    home.join(".mergeloop/session/session.json")
}

#[tokio::test]
async fn login_keeps_a_session_that_later_calls_send() {
    let fake = fake::Server::start().await;
    let home = tempfile::tempdir().unwrap();
    let server = Server::parse(&fake.url).unwrap();

    let who = login::login(&server, &sessions(home.path()), browser)
        .await
        .unwrap();
    assert_eq!(who, fake::LOGIN);

    let client = Client::signed_in(&server, &sessions(home.path())).unwrap();
    let answer = client.json("list_repos", json!({})).await.unwrap();
    assert_eq!(answer, json!({ "tool": "list_repos", "as": "session" }));
    assert_eq!(fake.bearers().last().unwrap(), "session-1");
}

#[tokio::test]
async fn login_asks_with_a_short_code_and_gives_nothing_when_denied() {
    let fake = fake::Server::start().await;
    let home = tempfile::tempdir().unwrap();
    let path = sessions(home.path());
    let server = Server::parse(&fake.url).unwrap();
    let mut link = String::new();
    login::login(&server, &path, |url: &str| {
        link = url.to_owned();
        browser(url)
    })
    .await
    .unwrap();
    let code = reqwest::Url::parse(&link)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "user_code")
        .map(|(_, v)| v.into_owned())
        .unwrap();
    assert!(
        code.len() == 9 && code.as_bytes()[4] == b'-',
        "a short code to read aloud: {code}"
    );

    let err = login::login(&server, &path, answering("deny"))
        .await
        .unwrap_err();
    assert!(format!("{err:#}").contains("denied"), "{err:#}");
    // The session from the first sign-in is still the one kept.
    assert_eq!(
        Client::signed_in(&server, &path)
            .unwrap()
            .json("list_repos", json!({}))
            .await
            .unwrap(),
        json!({ "tool": "list_repos", "as": "session" })
    );
    assert_eq!(fake.bearers().last().unwrap(), "session-1");
}

#[cfg(unix)]
#[tokio::test]
async fn only_its_owner_may_read_the_session() {
    use std::os::unix::fs::PermissionsExt;
    let fake = fake::Server::start().await;
    let home = tempfile::tempdir().unwrap();
    let path = sessions(home.path());
    // A file left readable by others, as an old one might be, is replaced by one that is not.
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "{}").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

    login::login(&Server::parse(&fake.url).unwrap(), &path, browser)
        .await
        .unwrap();

    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().unwrap()), 0o700);
}

#[tokio::test]
async fn a_session_belongs_to_the_server_it_was_made_on() {
    let fake = fake::Server::start().await;
    let other = fake::Server::start().await;
    let home = tempfile::tempdir().unwrap();
    let path = sessions(home.path());

    login::login(&Server::parse(&fake.url).unwrap(), &path, browser)
        .await
        .unwrap();

    let err = Client::signed_in(&Server::parse(&other.url).unwrap(), &path).unwrap_err();
    assert!(format!("{err:#}").contains("mergeloop login"), "{err:#}");
    // A trailing slash names the same server.
    Client::signed_in(&Server::parse(&format!("{}/", fake.url)).unwrap(), &path).unwrap();
}

#[tokio::test]
async fn logout_ends_the_session_on_the_server_and_forgets_it() {
    let fake = fake::Server::start().await;
    let home = tempfile::tempdir().unwrap();
    let path = sessions(home.path());
    let server = Server::parse(&fake.url).unwrap();
    login::login(&server, &path, browser).await.unwrap();
    assert!(fake.takes("session-1"));

    assert!(login::logout(&server, &path).await.unwrap());

    assert!(!fake.takes("session-1"));
    assert!(Client::signed_in(&server, &path).is_err());
    assert!(
        !std::fs::read_to_string(&path)
            .unwrap()
            .contains("session-1")
    );
    // Logging out again is not an error: there is nothing to forget.
    assert!(!login::logout(&server, &path).await.unwrap());
}

#[tokio::test]
async fn a_session_the_server_rejects_says_to_log_in_once() {
    let fake = fake::Server::start().await;
    let home = tempfile::tempdir().unwrap();
    let path = sessions(home.path());
    let server = Server::parse(&fake.url).unwrap();
    login::login(&server, &path, browser).await.unwrap();
    fake.seen.lock().unwrap().sessions.clear();
    let before = fake.bearers().len();

    let client = Client::signed_in(&server, &path).unwrap();
    let err = client.json("list_repos", json!({})).await.unwrap_err();

    let said = format!("{err:#}");
    assert!(said.contains("run `mergeloop login`"), "{said}");
    assert!(
        !said.contains("session-1"),
        "the session is never shown: {said}"
    );
    assert_eq!(fake.bearers().len(), before + 1, "no retry");
}
