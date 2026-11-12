//! `mergeloop`, the command an agent lands a PR with. `mergeloop [pr]` queues the PR, waits until
//! it needs something of the agent, prints that need with its brief, and exits with a code that
//! says what kind it is. The agent does what the brief says, then runs the command the need gives
//! as `then`. `mergeloop call <tool> [json]` runs any MCP tool of the server. `mergeloop login`
//! signs in, for the calls only a signed-in user may make.
//!
//! What a PR needs, and what to do about it, is the server's, through its MCP endpoint, so this
//! stays a thin client. The only state it keeps is the session `login` makes.

use std::io::Read;
use std::process::ExitCode;

use anyhow::{Context, Result, bail};
use mergeloop_cli::client::{Client, Server};
use mergeloop_cli::land::{PrRef, land, resolve};
use mergeloop_cli::{login, session};
use serde_json::{Value, json};

const USAGE: &str = "\
Land a PR through MergeLoop's merge queue.

Usage:
  mergeloop [PR] [--after ID]    Queue the PR, wait until it needs you, print what to do
  mergeloop call TOOL [JSON|-]   Run one of MergeLoop's MCP tools, with JSON arguments
  mergeloop login                Sign in to MergeLoop in the browser, and keep the session
  mergeloop logout               End the session, and forget it

PR is 123, owner/name#123, or a PR's URL. Without one, the current branch's PR.
Do what a need's `brief` says, then run its `then`, until mergeloop exits 10.

Exit codes: 0 a need for you, 10 merged, 11 ejected, 12 waiting on a human, 2 an error.

A PR and `call` act as the first of: MERGELOOP_TOKEN; the session from `mergeloop login`
for MERGELOOP_URL's server; `gh auth token`. A session reaches the tools for a whole
workspace, a GitHub token only single repos. A session the server no longer takes is an
error: run `mergeloop login` again.

Files:
  ~/.mergeloop/session.json   The sessions, one per server, readable by you only.

Environment:
  MERGELOOP_TOKEN   A GitHub token, sent instead of the session.
  MERGELOOP_URL     The MergeLoop server. Default https://mergeloop.ai.";

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            eprintln!("mergeloop: {e:#}");
            ExitCode::from(2)
        }
    }
}

#[derive(Debug, PartialEq)]
enum Cmd {
    Land {
        pr: Option<PrRef>,
        after: Option<String>,
    },
    Call {
        tool: String,
        args: Option<String>,
    },
    Login,
    Logout,
    Help,
    Version,
}

async fn run(args: &[String]) -> Result<u8> {
    let server = Server::from_env();
    match parse(args)? {
        Cmd::Help => {
            println!("{USAGE}");
            Ok(0)
        }
        Cmd::Version => {
            println!("mergeloop {}", env!("CARGO_PKG_VERSION"));
            Ok(0)
        }
        Cmd::Login => {
            let who = login::login(&server, &session::path()?, login::open_browser).await?;
            eprintln!("mergeloop: signed in to {} as {who}", server.url());
            Ok(0)
        }
        Cmd::Logout => {
            if login::logout(&server, &session::path()?).await? {
                eprintln!("mergeloop: signed out of {}", server.url());
            } else {
                eprintln!("mergeloop: not signed in to {}", server.url());
            }
            Ok(0)
        }
        Cmd::Call { tool, args } => {
            let args = call_args(args.as_deref())?;
            let client = Client::for_command(server, &session::path()?)?;
            let (text, is_error) = client.tool(&tool, args).await?;
            println!("{text}");
            Ok(if is_error { 1 } else { 0 })
        }
        Cmd::Land { pr, after } => {
            let (repo, number) = resolve(pr)?;
            let client = Client::for_command(server, &session::path()?)?;
            land(&client, &repo, number, after).await
        }
    }
}

/// A call's arguments: none, the given JSON, or JSON on stdin for `-`.
fn call_args(given: Option<&str>) -> Result<Value> {
    match given {
        None => Ok(json!({})),
        Some("-") => {
            let mut s = String::new();
            std::io::stdin().read_to_string(&mut s)?;
            serde_json::from_str(&s).context("the arguments on stdin are not JSON")
        }
        Some(s) => serde_json::from_str(s).context("the arguments are not JSON"),
    }
}

fn parse(args: &[String]) -> Result<Cmd> {
    let mut args = args.iter().map(String::as_str);
    let mut pr = None;
    let mut after = None;
    while let Some(arg) = args.next() {
        let first = pr.is_none() && after.is_none();
        match arg {
            "-h" | "--help" | "help" => return Ok(Cmd::Help),
            "-V" | "--version" => return Ok(Cmd::Version),
            "call" if first => {
                let tool = args.next().context("call needs a tool's name")?;
                let rest = args.next().map(str::to_owned);
                if args.next().is_some() {
                    bail!("call takes a tool and one JSON argument");
                }
                return Ok(Cmd::Call {
                    tool: tool.to_owned(),
                    args: rest,
                });
            }
            "login" | "logout" if first => {
                if let Some(extra) = args.next() {
                    bail!("{arg} takes no arguments, not {extra}\n\n{USAGE}");
                }
                return Ok(if arg == "login" {
                    Cmd::Login
                } else {
                    Cmd::Logout
                });
            }
            "--after" => after = Some(args.next().context("--after needs a need's id")?.into()),
            _ if arg.starts_with('-') => bail!("unknown option {arg}\n\n{USAGE}"),
            _ if pr.is_none() => pr = Some(PrRef::parse(arg)?),
            _ => bail!("one PR at a time\n\n{USAGE}"),
        }
    }
    Ok(Cmd::Land { pr, after })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_owned).collect()
    }

    #[test]
    fn the_command_is_the_loop_a_call_or_a_sign_in() {
        assert_eq!(
            parse(&args("")).unwrap(),
            Cmd::Land {
                pr: None,
                after: None
            }
        );
        assert_eq!(
            parse(&args("o/r#5 --after abc")).unwrap(),
            Cmd::Land {
                pr: Some(PrRef {
                    repo: Some("o/r".into()),
                    number: 5
                }),
                after: Some("abc".into()),
            }
        );
        assert_eq!(
            parse(&args("call pull_status {}")).unwrap(),
            Cmd::Call {
                tool: "pull_status".into(),
                args: Some("{}".into())
            }
        );
        assert_eq!(parse(&args("login")).unwrap(), Cmd::Login);
        assert_eq!(parse(&args("logout")).unwrap(), Cmd::Logout);
        assert_eq!(parse(&args("--help")).unwrap(), Cmd::Help);
        assert!(parse(&args("5 6")).is_err());
        assert!(parse(&args("--bogus")).is_err());
        assert!(parse(&args("call")).is_err());
        assert!(parse(&args("login now")).is_err());
        assert!(parse(&args("5 login")).is_err());
    }
}
