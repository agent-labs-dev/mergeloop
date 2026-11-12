//! `mergeloop login`: signs in through the OAuth the server offers MCP clients
//! (`crates/web/src/oauth.rs`), as a native app does (RFC 8252): the browser signs in with
//! GitHub, and the server sends a one-time code back to a listener on this machine, which trades
//! it, with the PKCE verifier only this process knows, for a MergeLoop session.
//!
//! The server keeps nothing about a client, so the command does not register one.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use reqwest::Url;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

use crate::client::{self, Client, Credential, Server};
use crate::session::{self, Secret};

/// How long the browser has to sign in. The server's sign-in link lasts 10 minutes.
const SIGN_IN_WAIT: Duration = Duration::from_secs(600);
/// How long one connection to the listener has to send its request.
const REQUEST_WAIT: Duration = Duration::from_secs(10);
/// The most of a request the listener reads: a callback's line and headers.
const REQUEST_MAX: usize = 16 * 1024;

/// Signs in to `server` in the browser, which `browser` opens at the sign-in link, and keeps
/// the session in `sessions`. Returns the GitHub login signed in.
pub async fn login(
    server: &Server,
    sessions: &Path,
    browser: impl FnOnce(&str) -> Result<()>,
) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("listen on 127.0.0.1 for the sign-in")?;
    let port = listener.local_addr()?.port();
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let state = random_token(16);
    let verifier = random_token(32);
    let link = sign_in_link(server, &redirect, &state, &verifier)?;
    eprintln!("mergeloop: sign in at {link}");
    if let Err(e) = browser(link.as_str()) {
        eprintln!("mergeloop: could not open a browser ({e:#}): open the link yourself");
    }
    let code = tokio::time::timeout(SIGN_IN_WAIT, callback(listener, port, state))
        .await
        .context("the sign-in took over 10 minutes: run `mergeloop login` again")??;
    let session = trade(server, &code, &verifier, &redirect).await?;
    let login = who(server, &session).await?;
    session::keep(sessions, server, &session)?;
    Ok(login)
}

/// The server's sign-in page for this listener, with the `state` and PKCE challenge.
fn sign_in_link(server: &Server, redirect: &str, state: &str, verifier: &str) -> Result<Url> {
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    Url::parse_with_params(
        &format!("{}/oauth/authorize", server.url()),
        [
            ("response_type", "code"),
            ("redirect_uri", redirect),
            ("state", state),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
        ],
    )
    .with_context(|| format!("{} is not a URL", server.url()))
}

/// The GitHub login a new session is for: the proof it works before it is kept.
async fn who(server: &Server, session: &Secret) -> Result<String> {
    let client = Client::new(server.clone(), Credential::Session(session.clone()))?;
    let answer = client.json("get_session", json!({})).await?;
    match answer["user"]["login"].as_str() {
        Some(login) => Ok(login.to_owned()),
        None => bail!("{} took the sign-in, but has no user for it", server.url()),
    }
}

/// Ends the server's session and forgets it. `false` when there was none. Forgotten even when
/// the server cannot be reached: it then ends on its own, after 30 days unused.
pub async fn logout(server: &Server, sessions: &Path) -> Result<bool> {
    let Some(session) = session::forget(sessions, server)? else {
        return Ok(false);
    };
    let client = Client::new(server.clone(), Credential::Session(session))?;
    if let Err(e) = client.end_session().await {
        eprintln!("mergeloop: forgot the session, but could not end it on the server: {e:#}");
    }
    Ok(true)
}

/// Opens the link in the user's browser. Not the session: it is never in an argument.
pub fn open_browser(link: &str) -> Result<()> {
    let opener = if cfg!(target_os = "macos") {
        "open"
    } else {
        "xdg-open"
    };
    let status = std::process::Command::new(opener)
        .arg(link)
        .status()
        .with_context(|| format!("run `{opener}`"))?;
    if !status.success() {
        bail!("`{opener}` failed: {status}");
    }
    Ok(())
}

/// Trades the one-time code and the verifier for the session.
async fn trade(server: &Server, code: &str, verifier: &str, redirect: &str) -> Result<Secret> {
    let res = client::http()?
        .post(format!("{}/oauth/token", server.url()))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("code_verifier", verifier),
            ("redirect_uri", redirect),
        ])
        .send()
        .await
        .with_context(|| format!("reach {}", server.url()))?;
    let status = res.status();
    let body: Value = res.json().await.unwrap_or_default();
    match body["access_token"].as_str() {
        Some(token) if status.is_success() => Ok(Secret::new(token.to_owned())),
        _ => bail!(
            "{} did not sign you in ({status}): {}",
            server.url(),
            body["error_description"]
                .as_str()
                .unwrap_or("no reason given")
        ),
    }
}

/// Waits for the browser's callback with this sign-in's `state`, and returns its code. Other
/// requests are refused, and the wait goes on: only the right callback ends it.
async fn callback(listener: TcpListener, port: u16, state: String) -> Result<String> {
    let (tx, mut rx) = mpsc::channel(1);
    let accepting = tokio::spawn(async move {
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let (tx, state) = (tx.clone(), state.clone());
            // Each on its own: a connection that sends nothing, as a browser's preconnect, must
            // not hold up the callback.
            tokio::spawn(async move {
                if let Some(verdict) = answer(stream, port, &state).await {
                    let _ = tx.send(verdict).await;
                }
            });
        }
    });
    let verdict = rx.recv().await;
    accepting.abort();
    match verdict {
        Some(Callback::Code(code)) => Ok(code),
        Some(Callback::Failed(why)) => bail!("the sign-in failed: {why}"),
        None => bail!("the sign-in listener stopped"),
    }
}

/// Reads one request, answers it, and returns what it settles, if anything.
async fn answer(mut stream: TcpStream, port: u16, state: &str) -> Option<Callback> {
    let head = tokio::time::timeout(REQUEST_WAIT, read_head(&mut stream))
        .await
        .ok()??;
    let verdict = judge(&head, port, state);
    let (status, page) = match &verdict {
        Ok(Callback::Code(_)) => ("200 OK", "Signed in to MergeLoop. You can close this tab."),
        Ok(Callback::Failed(_)) => ("200 OK", "The sign-in failed. The terminal says why."),
        Err(refused) => ("400 Bad Request", *refused),
    };
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: text/plain; charset=utf-8\r\ncontent-length: {}\r\ncache-control: no-store\r\nconnection: close\r\n\r\n{page}",
        page.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
    verdict.ok()
}

/// The request's line and headers, or `None` when it sent none in time or too many.
async fn read_head(stream: &mut TcpStream) -> Option<String> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 || buf.len() + n > REQUEST_MAX {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    String::from_utf8(buf).ok()
}

/// What a callback settles.
#[derive(Debug, PartialEq)]
enum Callback {
    Code(String),
    /// The server's error for this sign-in.
    Failed(String),
}

/// Whether the request is this sign-in's callback: a GET of `/callback`, as a browser sends it
/// to this listener (its own `Host`, which a page on another site cannot fake by DNS; no
/// `Origin`, which a page's script would send), with this sign-in's `state`, which nobody else
/// has. `Err` is why a request is refused.
fn judge(head: &str, port: u16, state: &str) -> Result<Callback, &'static str> {
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap_or_default();
    let mut parts = line.split(' ');
    let (Some("GET"), Some(target)) = (parts.next(), parts.next()) else {
        return Err("Only the sign-in's callback is answered here.");
    };
    let header = |name: &str| {
        head.split("\r\n").skip(1).find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
    };
    if header("host") != Some(&format!("127.0.0.1:{port}")) || header("origin").is_some() {
        return Err("Only the sign-in's callback is answered here.");
    }
    let Ok(url) = Url::parse(&format!("http://127.0.0.1:{port}{target}")) else {
        return Err("Only the sign-in's callback is answered here.");
    };
    if url.path() != "/callback" {
        return Err("Only the sign-in's callback is answered here.");
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.into_owned())
    };
    let given = param("state").unwrap_or_default();
    if !same(given.as_bytes(), state.as_bytes()) {
        return Err("This is not the sign-in the command started: run `mergeloop login` again.");
    }
    if let Some(error) = param("error") {
        return Ok(Callback::Failed(
            param("error_description").unwrap_or(error),
        ));
    }
    match param("code").filter(|c| !c.is_empty()) {
        Some(code) => Ok(Callback::Code(code)),
        None => Ok(Callback::Failed("the server sent no code".into())),
    }
}

/// Equal, in a time that does not say how much of `a` matched.
fn same(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// A random URL-safe string with `bytes` bytes of entropy.
fn random_token(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("the OS has no random source");
    URL_SAFE_NO_PAD.encode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORT: u16 = 4321;

    fn request(target: &str, extra: &str) -> String {
        format!("GET {target} HTTP/1.1\r\nHost: 127.0.0.1:{PORT}\r\n{extra}\r\n")
    }

    #[test]
    fn the_callback_with_this_sign_ins_state_gives_its_code() {
        assert_eq!(
            judge(&request("/callback?code=c1&state=s1", ""), PORT, "s1"),
            Ok(Callback::Code("c1".into()))
        );
        assert_eq!(
            judge(
                &request(
                    "/callback?error=access_denied&error_description=no%20way&state=s1",
                    ""
                ),
                PORT,
                "s1"
            ),
            Ok(Callback::Failed("no way".into()))
        );
    }

    #[test]
    fn anything_else_is_refused() {
        let refused = |head: &str| judge(head, PORT, "s1").is_err();
        // Another sign-in's, or none.
        assert!(refused(&request("/callback?code=c1&state=s2", "")));
        assert!(refused(&request("/callback?code=c1", "")));
        assert!(refused(&request("/callback?code=c1&state=s", "")));
        // Another path or method.
        assert!(refused(&request("/favicon.ico?state=s1&code=c1", "")));
        assert!(refused(
            "POST /callback?code=c1&state=s1 HTTP/1.1\r\nHost: 127.0.0.1:4321\r\n\r\n"
        ));
        // A page's script, or a name that resolves here.
        assert!(refused(&request(
            "/callback?code=c1&state=s1",
            "Origin: https://evil.example\r\n"
        )));
        assert!(refused(&format!(
            "GET /callback?code=c1&state=s1 HTTP/1.1\r\nHost: evil.example:{PORT}\r\n\r\n"
        )));
        assert!(refused("GET /callback?code=c1&state=s1 HTTP/1.1\r\n\r\n"));
    }

    /// Sends a raw request to the listener and returns the answer's status line.
    async fn send(port: u16, head: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        stream.write_all(head.as_bytes()).await.unwrap();
        let mut out = String::new();
        stream.read_to_string(&mut out).await.unwrap();
        out.lines().next().unwrap_or_default().to_owned()
    }

    #[tokio::test]
    async fn the_listener_waits_out_wrong_requests_and_silent_connections() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let waiting = tokio::spawn(callback(listener, port, "s1".into()));

        // A connection that sends nothing, as a browser's preconnect.
        let _silent = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        let host = format!("Host: 127.0.0.1:{port}\r\n");
        let wrong = format!("GET /callback?code=bad&state=other HTTP/1.1\r\n{host}\r\n");
        assert!(send(port, &wrong).await.contains("400"));
        let right = format!("GET /callback?code=good&state=s1 HTTP/1.1\r\n{host}\r\n");
        assert!(send(port, &right).await.contains("200"));

        assert_eq!(waiting.await.unwrap().unwrap(), "good");
    }
}
