//! A MergeLoop server on localhost, as far as the command reaches it: the MCP client's OAuth
//! (`crates/web/src/oauth.rs`), `/mcp` and `/auth/logout`. It keeps what it was sent, so a test
//! can say who called.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// The GitHub token the fake takes.
pub const GITHUB_TOKEN: &str = "ghp_fake";
/// The token `mint_build_token` gives for `push`: the daemon's alone.
pub const PUSH_TOKEN: &str = "ghs_fake_push";
/// The token `mint_build_token` gives for `read`: what an agent may hold.
pub const READ_TOKEN: &str = "ghs_fake_read";
/// Who a session signs in as.
pub const LOGIN: &str = "ana";

/// The build lane, as far as the daemon reaches it.
#[derive(Default)]
pub struct Lane {
    /// The tasks `claim_build` gives, in order.
    pub tasks: VecDeque<Value>,
    /// What `list_builds` lists as held by the caller.
    pub held: Vec<Value>,
    /// Each `release_build`: the build, and why it is stuck.
    pub released: Vec<(u64, Option<String>)>,
    /// Each `open_build_pr`: the build, the title, the body.
    pub opened: Vec<(u64, String, String)>,
    /// Refusals to answer once each, in place of the tool's answer: the tool, the refusal's
    /// kind as `_meta.refusal` names it, its status, and its text.
    pub refusals: Vec<(String, String, u16, String)>,
    /// The needs `next_need` gives a daemon for each PR, in order, each once. Then the last
    /// one given stays the PR's need, as the server's stays until something changes it.
    pub needs: HashMap<u64, VecDeque<Value>>,
    /// Each PR's need as it is now: the last one `next_need` gave.
    pub current: HashMap<u64, Value>,
    /// Each `report_build`: the build, and its standing.
    pub reports: Vec<(u64, String)>,
    /// What `get_build_task` gives for each build.
    pub held_tasks: HashMap<u64, Value>,
    /// The PR's review threads, as GitHub's GraphQL lists them: id, resolved.
    pub threads: Vec<(String, bool)>,
    /// Each call to GitHub's GraphQL: its bearer, query and variables.
    pub graphql: Vec<(String, String, Value)>,
    /// GraphQL calls GitHub refuses: those whose query has the text, so many more times.
    pub graphql_refusals: Vec<(String, u32)>,
    /// GraphQL calls GitHub does but whose answer is lost on the way: those whose query has
    /// the text, so many more times. They answer 502.
    pub graphql_lost: Vec<(String, u32)>,
    /// The replies in each review thread, oldest first: all the bot's, as only it replies here.
    pub comments: HashMap<String, Vec<String>>,
}

impl Lane {
    /// How many builds the server heard end: PRs opened and builds released.
    pub fn ended(&self) -> usize {
        self.opened.len() + self.released.len()
    }

    /// Whether a build was reported merged.
    pub fn merged(&self, build: u64) -> bool {
        self.reports.iter().any(|r| *r == (build, "merged".into()))
    }
}

/// A need as `next_need` gives it.
pub fn need(needs: &str, id: &str, brief: &str) -> Value {
    json!({ "needs": needs, "id": id, "brief": brief, "exit_code": 0 })
}

#[derive(Default)]
pub struct Seen {
    /// Each `/mcp` call: its bearer and tool.
    pub calls: Vec<(String, String)>,
    /// Each `/mcp` call's arguments.
    pub args: Vec<Value>,
    pub lane: Lane,
    /// Sessions the server takes.
    pub sessions: HashSet<String>,
    /// Each one-time code: its PKCE challenge.
    codes: HashMap<String, String>,
    issued: u32,
}

#[derive(Clone)]
pub struct Server {
    pub url: String,
    pub seen: Arc<Mutex<Seen>>,
}

impl Server {
    pub async fn start() -> Self {
        let seen = Arc::new(Mutex::new(Seen::default()));
        let app = Router::new()
            .route("/oauth/authorize", get(authorize))
            .route("/oauth/token", post(token))
            .route("/mcp", post(mcp))
            .route("/graphql", post(graphql))
            .route("/auth/logout", post(logout))
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Server { url, seen }
    }

    /// The tools called, in order.
    pub fn tools(&self) -> Vec<String> {
        let seen = self.seen.lock().unwrap();
        seen.calls.iter().map(|(_, t)| t.clone()).collect()
    }

    /// The bearers of the `/mcp` calls, in order.
    pub fn bearers(&self) -> Vec<String> {
        let seen = self.seen.lock().unwrap();
        seen.calls.iter().map(|(b, _)| b.clone()).collect()
    }

    pub fn takes(&self, session: &str) -> bool {
        self.seen.lock().unwrap().sessions.contains(session)
    }

    /// The tools called, in order, but `get_session` and `list_builds`.
    pub fn lane_tools(&self) -> Vec<String> {
        self.tools()
            .into_iter()
            .filter(|t| t != "get_session" && t != "list_builds")
            .collect()
    }

    /// The arguments of each call to `tool`.
    pub fn args_of(&self, tool: &str) -> Vec<Value> {
        let seen = self.seen.lock().unwrap();
        seen.calls
            .iter()
            .zip(&seen.args)
            .filter(|((_, t), _)| t == tool)
            .map(|(_, a)| a.clone())
            .collect()
    }

    /// Scripts the needs `next_need` gives a daemon for PR `pr`.
    pub fn script(&self, pr: u64, needs: Vec<Value>) {
        let mut seen = self.seen.lock().unwrap();
        seen.lane.needs.insert(pr, needs.into());
    }

    /// Adds a task for `claim_build` to give.
    pub fn add_task(&self, task: Value) {
        self.seen.lock().unwrap().lane.tasks.push_back(task);
    }

    /// Refuses the next call to `tool`, as the server's lane does.
    pub fn refuse(&self, tool: &str, kind: &str, status: u16, text: &str) {
        let mut seen = self.seen.lock().unwrap();
        seen.lane
            .refusals
            .push((tool.into(), kind.into(), status, text.into()));
    }

    pub fn lane<T>(&self, f: impl FnOnce(&Lane) -> T) -> T {
        f(&self.seen.lock().unwrap().lane)
    }

    /// A session the server takes, as if signed in before.
    pub fn add_session(&self, session: &str) {
        let mut seen = self.seen.lock().unwrap();
        seen.sessions.insert(session.to_owned());
    }
}

type Shared = State<Arc<Mutex<Seen>>>;

/// As the real one: a loopback redirect, `code` with S256. GitHub's part is skipped: the user
/// signs in at once.
async fn authorize(State(seen): Shared, Query(q): Query<HashMap<String, String>>) -> Response {
    let redirect = q.get("redirect_uri").cloned().unwrap_or_default();
    if !redirect.starts_with("http://127.0.0.1:") {
        return (StatusCode::BAD_REQUEST, "no loopback redirect_uri").into_response();
    }
    if q.get("response_type").map(String::as_str) != Some("code")
        || q.get("code_challenge_method").map(String::as_str) != Some("S256")
    {
        return Redirect::to(&format!("{redirect}?error=invalid_request")).into_response();
    }
    let mut seen = seen.lock().unwrap();
    seen.issued += 1;
    let code = format!("code-{}", seen.issued);
    let challenge = q.get("code_challenge").cloned().unwrap_or_default();
    seen.codes.insert(code.clone(), challenge);
    let state = q.get("state").cloned().unwrap_or_default();
    Redirect::to(&format!("{redirect}?code={code}&state={state}")).into_response()
}

async fn token(State(seen): Shared, Form(f): Form<HashMap<String, String>>) -> Response {
    let mut seen = seen.lock().unwrap();
    let challenge = f.get("code").and_then(|c| seen.codes.remove(c));
    let verifier = f.get("code_verifier").cloned().unwrap_or_default();
    let given = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    if f.get("grant_type").map(String::as_str) != Some("authorization_code")
        || challenge.as_deref() != Some(given.as_str())
    {
        let body = json!({ "error": "invalid_grant", "error_description": "This sign-in expired, or was used." });
        return (StatusCode::BAD_REQUEST, Json(body)).into_response();
    }
    let session = format!("session-{}", seen.issued);
    seen.sessions.insert(session.clone());
    Json(json!({ "access_token": session, "token_type": "Bearer" })).into_response()
}

fn bearer(headers: &HeaderMap) -> String {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default()
        .to_owned()
}

/// A tool's answer says which tool ran, and whether as a sign-in or a GitHub token. A daemon
/// that asks for a need when none is scripted waits a moment, as the server would wait longer.
async fn mcp(State(seen): Shared, headers: HeaderMap, Json(message): Json<Value>) -> Response {
    let args = &message["params"]["arguments"];
    if message["params"]["name"] == "next_need" && args["for_daemon"] == true {
        let number = args["number"].as_u64().unwrap_or_default();
        let scripted = seen
            .lock()
            .unwrap()
            .lane
            .needs
            .get(&number)
            .is_some_and(|needs| !needs.is_empty());
        if !scripted {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    answer(seen, headers, message)
}

fn answer(seen: Arc<Mutex<Seen>>, headers: HeaderMap, message: Value) -> Response {
    let bearer = bearer(&headers);
    let tool = message["params"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    let mut seen = seen.lock().unwrap();
    let args = &message["params"]["arguments"];
    seen.calls.push((bearer.clone(), tool.clone()));
    seen.args.push(args.clone());
    let signed_in = seen.sessions.contains(&bearer);
    if !signed_in && bearer != GITHUB_TOKEN {
        return (StatusCode::UNAUTHORIZED, "Sign in, or pass a GitHub token").into_response();
    }
    if let Some(i) = seen.lane.refusals.iter().position(|r| r.0 == tool) {
        let (_, kind, status, text) = seen.lane.refusals.remove(i);
        let result = json!({
            "content": [{ "type": "text", "text": text }], "isError": true,
            "_meta": { "status": status, "refusal": kind },
        });
        return Json(json!({ "jsonrpc": "2.0", "id": message["id"], "result": result }))
            .into_response();
    }
    let text = match tool.as_str() {
        "get_session" if signed_in => json!({
            "status": "signed_in", "user": { "login": LOGIN },
            "workspaces": [{ "installation_id": 1, "account": "o" }],
        }),
        "claim_build" => seen.lane.tasks.pop_front().unwrap_or(Value::Null),
        // The held builds of the daemon that asks, as the server answers `mine` with `daemon`.
        "list_builds" => Value::from(
            seen.lane
                .held
                .iter()
                .filter(|b| b["daemon"] == args["daemon"])
                .cloned()
                .collect::<Vec<_>>(),
        ),
        "heartbeat_build" => json!({ "id": args["build"], "status": "claimed" }),
        "mint_build_token" => {
            let token = match args["scope"].as_str() {
                Some("push") => PUSH_TOKEN,
                Some("read") => READ_TOKEN,
                _ => "no scope",
            };
            json!({ "token": token, "expires_at": "2026-11-12T13:00:00Z" })
        }
        "open_build_pr" => {
            let build = args["build"].as_u64().unwrap_or_default();
            let said = |k: &str| args[k].as_str().unwrap_or_default().to_owned();
            seen.lane.opened.push((build, said("title"), said("body")));
            json!({ "id": build, "status": "landing", "pr": 100 + build })
        }
        // The next scripted need, else the PR's need as it is now, unless the daemon saw it:
        // then a wait that names it, as the server's "nothing new since" does.
        "next_need" if args["for_daemon"] == true => {
            let number = args["number"].as_u64().unwrap_or_default();
            let lane = &mut seen.lane;
            if let Some(next) = lane.needs.get_mut(&number).and_then(VecDeque::pop_front) {
                lane.current.insert(number, next.clone());
                next
            } else {
                match lane.current.get(&number) {
                    Some(now) if now["needs"] != "wait" && args["after"] != now["id"] => {
                        now.clone()
                    }
                    Some(now) => need("wait", now["id"].as_str().unwrap_or("w"), "Nothing new"),
                    None => need(
                        "wait",
                        args["after"].as_str().unwrap_or("w"),
                        "Nothing for you yet",
                    ),
                }
            }
        }
        "report_build" => {
            let build = args["build"].as_u64().unwrap_or_default();
            let standing = args["standing"].as_str().unwrap_or_default().to_owned();
            seen.lane.reports.push((build, standing));
            json!({ "id": build })
        }
        "get_build_task" => {
            let build = args["build"].as_u64().unwrap_or_default();
            seen.lane
                .held_tasks
                .get(&build)
                .cloned()
                .unwrap_or(Value::Null)
        }
        "queue_pull" => json!({ "message": "queued" }),
        "release_build" => {
            let build = args["build"].as_u64().unwrap_or_default();
            let stuck = args["stuck"].as_str().map(str::to_owned);
            seen.lane.released.push((build, stuck));
            json!({ "id": build, "status": "ready" })
        }
        // A PR not queued yet, then threads to work once it is.
        "next_need" if args["after"] == "q1" => json!({
            "needs": "threads", "id": "t1", "repo": args["repo"], "number": args["number"],
            "brief": "Work the threads.", "then": "mergeloop o/r#5 --after t1", "exit_code": 0,
        }),
        "next_need" => json!({ "needs": "queue", "id": "q1", "exit_code": 0 }),
        _ => json!({ "tool": tool, "as": if signed_in { "session" } else { "github" } }),
    };
    let result =
        json!({ "content": [{ "type": "text", "text": text.to_string() }], "isError": false });
    Json(json!({ "jsonrpc": "2.0", "id": message["id"], "result": result })).into_response()
}

/// GitHub's GraphQL, as far as the daemon reaches it: a PR's review threads, a reply in one, and
/// resolving one.
async fn graphql(State(seen): Shared, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    let mut seen = seen.lock().unwrap();
    let query = body["query"].as_str().unwrap_or_default().to_owned();
    let vars = body["variables"].clone();
    seen.lane
        .graphql
        .push((bearer(&headers), query.clone(), vars.clone()));
    let refused = seen
        .lane
        .graphql_refusals
        .iter_mut()
        .find(|(text, times)| *times > 0 && query.contains(text.as_str()));
    if let Some((_, times)) = refused {
        *times -= 1;
        let errors =
            json!([{ "type": "FORBIDDEN", "message": "Resource not accessible by integration" }]);
        return Json(json!({ "data": null, "errors": errors })).into_response();
    }
    let lost = seen
        .lane
        .graphql_lost
        .iter_mut()
        .find(|(text, times)| *times > 0 && query.contains(text.as_str()))
        .map(|(_, times)| *times -= 1)
        .is_some();
    let data = if query.contains("reviewThreads") {
        // 100 a page, as GitHub's most; a page's cursor is the index it starts at.
        let from: usize = vars["after"].as_str().map_or(0, |a| a.parse().unwrap());
        let threads = &seen.lane.threads;
        let nodes: Vec<Value> = threads
            .iter()
            .skip(from)
            .take(100)
            .map(|(id, resolved)| {
                let said = seen.lane.comments.get(id).into_iter().flatten();
                let comments: Vec<Value> = said
                    .map(|body| json!({ "body": body, "viewerDidAuthor": true }))
                    .collect();
                json!({ "id": id, "isResolved": resolved, "comments": { "nodes": comments } })
            })
            .collect();
        let next = from + 100 < threads.len();
        let info =
            json!({ "hasNextPage": next, "endCursor": next.then(|| (from + 100).to_string()) });
        json!({ "repository": { "pullRequest": { "reviewThreads": { "nodes": nodes, "pageInfo": info } } } })
    } else if query.contains("resolveReviewThread") {
        let id = vars["thread"].as_str().unwrap_or_default();
        for thread in seen.lane.threads.iter_mut().filter(|t| t.0 == id) {
            thread.1 = true;
        }
        json!({ "resolveReviewThread": { "thread": { "id": id } } })
    } else {
        let thread = vars["thread"].as_str().unwrap_or_default().to_owned();
        let body = vars["body"].as_str().unwrap_or_default().to_owned();
        seen.lane.comments.entry(thread).or_default().push(body);
        json!({ "addPullRequestReviewThreadReply": { "comment": { "id": "c" } } })
    };
    if lost {
        return (StatusCode::BAD_GATEWAY, "Bad gateway").into_response();
    }
    Json(json!({ "data": data })).into_response()
}

async fn logout(State(seen): Shared, headers: HeaderMap) -> StatusCode {
    seen.lock().unwrap().sessions.remove(&bearer(&headers));
    StatusCode::NO_CONTENT
}
