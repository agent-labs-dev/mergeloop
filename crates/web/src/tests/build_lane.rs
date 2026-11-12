//! The build lane's routes, with GitHub faked: Ana and Ben may push to `o/r`, Cy may not.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::*;

const SECRET: &str = "whs";

/// What the fake GitHub has, and what it was asked to change.
#[derive(Default)]
struct Hub {
    /// `o/r`'s open issues with the `mergeloop` label, as the issues API lists them. A list
    /// over 100 is answered a page at a time.
    labeled: Vec<Value>,
    /// `o/r`'s config file.
    config: Option<String>,
    /// The writes: method, path, body.
    writes: Vec<(String, String, String)>,
    /// GitHub tokens that lost push access to `o/r`.
    revoked: Vec<String>,
    /// The bodies of scoped token mints.
    mints: Vec<Value>,
    /// `o/r`'s PRs.
    prs: Vec<Pr>,
    /// What opened each PR: the token, and the body sent.
    opened: Vec<(String, Value)>,
    /// Paths whose writes GitHub refuses.
    failing: Vec<String>,
    /// Each issue's events, as the issue events API lists them.
    events: Vec<(u64, Value)>,
    /// Installation tokens revoked.
    revoked_tokens: Vec<String>,
    /// Paths that were read.
    reads: Vec<String>,
    /// Acts before the answer to the first request whose path starts with its prefix: another
    /// call, put in the middle of the one that asked.
    hooks: Vec<(String, Hook)>,
}

type Hook = Box<dyn FnOnce() + Send>;

/// A PR in `o/r`.
#[derive(Clone)]
struct Pr {
    number: u64,
    head: String,
    head_repo: String,
    base: String,
    open: bool,
    merged: bool,
    author: String,
}

impl Pr {
    /// One the App opened from `head` into the default branch.
    fn bots(number: u64, head: &str) -> Self {
        Self {
            number,
            head: head.into(),
            head_repo: "o/r".into(),
            base: "trunk".into(),
            open: true,
            merged: false,
            author: "mp[bot]".into(),
        }
    }

    fn json(&self) -> Value {
        json!({
            "number": self.number, "html_url": format!("https://github.com/o/r/pull/{}", self.number),
            "state": if self.open { "open" } else { "closed" },
            "merged_at": if self.merged { json!("2026-11-12T12:00:00Z") } else { Value::Null },
            "user": { "login": self.author },
            "head": { "ref": self.head, "repo": { "full_name": self.head_repo } },
            "base": { "ref": self.base },
        })
    }
}

type Shared = Arc<Mutex<Hub>>;

fn issue(number: u64, assignees: &[&str]) -> Value {
    json!({
        "number": number, "title": format!("Issue {number}"), "body": "Build it.",
        "state": "open", "labels": [{ "name": "mergeloop" }],
        "assignees": assignees.iter().map(|a| json!({ "login": a })).collect::<Vec<_>>(),
    })
}

/// The `page`th page of 100 of `all`, for a list API.
fn page(all: &[Value], path: &str) -> (u16, String) {
    let n: usize = path.rsplit("page=").next().unwrap().parse().unwrap();
    let page: Vec<Value> = all.iter().skip((n - 1) * 100).take(100).cloned().collect();
    (200, Value::from(page).to_string())
}

async fn github(hub: Shared) -> GitHub {
    fake::github(move |call| {
        let mut hub = hub.lock().unwrap();
        if let Some(i) = hub.hooks.iter().position(|(p, _)| call.path.starts_with(p.as_str())) {
            (hub.hooks.remove(i).1)();
        }
        if call.method == "GET" {
            hub.reads.push(call.path.into());
        }
        let push = |yes: bool| (200, json!({ "permissions": { "push": yes } }).to_string());
        if call.method != "GET" && hub.failing.iter().any(|p| p == call.path) {
            return (422, r#"{"message":"Validation Failed"}"#.into());
        }
        let issues = "/repos/o/r/issues?labels=mergeloop&state=open&per_page=100&page=";
        match (call.method, call.path) {
            ("GET", "/rate_limit") => (200, "{}".into()),
            ("GET", "/repos/o/r") => push(
                matches!(call.token, "ghu_ana" | "ghu_ben")
                    && !hub.revoked.iter().any(|t| t == call.token),
            ),
            ("POST", "/app/installations/1/access_tokens") => {
                hub.mints.push(serde_json::from_str(call.body).unwrap());
                (201, json!({ "token": "ghs_push", "expires_at": "2026-11-12T13:00:00Z" }).to_string())
            }
            ("DELETE", "/installation/token") => {
                hub.revoked_tokens.push(call.token.into());
                (204, String::new())
            }
            ("GET", path) if path.starts_with("/repos/o/r/pulls?") => {
                let (query, branch) = path.split_once("&head=o:").unwrap();
                let open_only = query.contains("state=open");
                let found: Vec<Value> = hub
                    .prs
                    .iter()
                    .filter(|pr| pr.head == branch && (pr.open || !open_only))
                    .map(Pr::json)
                    .collect();
                (200, Value::from(found).to_string())
            }
            ("GET", path) if path.starts_with("/repos/o/r/pulls/") => {
                let number: u64 = path["/repos/o/r/pulls/".len()..].parse().unwrap();
                match hub.prs.iter().find(|pr| pr.number == number) {
                    Some(pr) => (200, pr.json().to_string()),
                    None => (404, "{}".into()),
                }
            }
            ("POST", "/repos/o/r/pulls") => {
                let body: Value = serde_json::from_str(call.body).unwrap();
                let head = body["head"].as_str().unwrap().to_owned();
                if hub.prs.iter().any(|pr| pr.open && pr.head == head) {
                    return (422, r#"{"message":"A pull request already exists"}"#.into());
                }
                let number = 40 + hub.prs.len() as u64;
                hub.prs.push(Pr::bots(number, &head));
                hub.opened.push((call.token.into(), body));
                (201, json!({ "number": number }).to_string())
            }
            ("GET", "/repos/o/y") => push(true),
            ("POST", "/graphql") => {
                let file = hub.config.as_ref().map(|text| json!({ "object": { "text": text } }));
                let target = json!({ "oid": "abc", "c0": file, "c1": null });
                let data = json!({ "repository": { "defaultBranchRef": { "name": "trunk", "target": target } } });
                (200, json!({ "data": data }).to_string())
            }
            ("GET", path) if path.starts_with(issues) => page(&hub.labeled, path),
            ("GET", "/repos/o/y/issues?labels=mergeloop&state=open&per_page=100&page=1") => {
                (200, "[]".into())
            }
            ("GET", path) if path.starts_with("/repos/o/r/issues/") && path.contains("/events?") => {
                let number: u64 = path["/repos/o/r/issues/".len()..].split('/').next().unwrap().parse().unwrap();
                let events: Vec<Value> = hub.events.iter().filter(|(n, _)| *n == number).map(|(_, e)| e.clone()).collect();
                page(&events, path)
            }
            ("GET", "/users/mp%5Bbot%5D") => (200, r#"{"id":900}"#.into()),
            ("GET", "/users/ana") => (200, r#"{"id":1}"#.into()),
            ("GET", "/users/cy") => (200, r#"{"id":2}"#.into()),
            ("GET", "/users/ben") => (200, r#"{"id":3}"#.into()),
            ("POST" | "DELETE", path) if path.starts_with("/repos/o/r/issues/") => {
                hub.writes
                    .push((call.method.into(), path.into(), call.body.into()));
                (201, "{}".into())
            }
            _ => (404, "{}".into()),
        }
    })
    .await
}

/// Runs `act` on the store from another thread and connection, while the route that called the
/// fake waits for its answer: how a test puts another call in the middle of one.
fn aside<F, Fut>(options: &sqlx::postgres::PgConnectOptions, act: F)
where
    F: FnOnce(PgStore) -> Fut + Send,
    Fut: std::future::Future<Output = ()>,
{
    std::thread::scope(|s| {
        s.spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let pool = PgPool::connect_with(options.clone()).await.unwrap();
                act(PgStore::migrate(pool).await.unwrap()).await;
            });
        })
        .join()
        .unwrap();
    });
}

/// Before GitHub answers the first request to a path starting with `prefix`, runs `act` on the
/// store as [`aside`] does.
fn hook<F, Fut>(hub: &Shared, pool: &PgPool, prefix: &str, act: F)
where
    F: FnOnce(PgStore) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()>,
{
    let options = (*pool.connect_options()).clone();
    let run: Hook = Box::new(move || aside(&options, act));
    hub.lock().unwrap().hooks.push((prefix.into(), run));
}

/// Ana, Cy and Ben signed in to workspace 1, whose repos `o/r` and `o/y` queue.
async fn lane(pool: PgPool, hub: &Shared) -> (Router, AppState) {
    let config = web_config(Some(app(Some(SECRET))), Arc::default());
    let mut state = signed_in_state(pool, config).await;
    state.github = github(hub.clone()).await;
    for login in ["ana", "cy", "ben"] {
        sign_in(&state, login, login, &format!("ghu_{login}")).await;
    }
    for repo in ["o/r", "o/y"] {
        let repo = RepoName::parse(repo).unwrap();
        state.store.set_has_config(1, &repo, true).await.unwrap();
    }
    (serve(state.clone()), state)
}

async fn turn_on(router: &Router) {
    let (status, _) = call(
        router,
        "ana",
        "PUT",
        "/settings/builds",
        json!({ "enabled": true }),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
}

/// `who` calls `/api/w/1{path}`.
async fn call(
    router: &Router,
    who: &str,
    method: &str,
    path: &str,
    body: Value,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method(method)
        .uri(format!("/api/w/1{path}"))
        .header("cookie", format!("mp_session={who}"))
        .header("content-type", "application/json")
        .body(match method {
            "GET" => Body::empty(),
            _ => Body::from(body.to_string()),
        })
        .unwrap();
    let res = send(router, req).await;
    let status = res.status();
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()));
    (status, value)
}

async fn claim(router: &Router, who: &str) -> Value {
    let (status, body) = call(router, who, "POST", "/builds/claim", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    body
}

async fn heartbeat(router: &Router, who: &str, claimed: &Value) -> StatusCode {
    let path = format!("/builds/{}/heartbeat", claimed["build"]);
    let body = json!({ "claim": claimed["claim"], "stage": "reviewing" });
    call(router, who, "POST", &path, body).await.0
}

async fn release(
    router: &Router,
    who: &str,
    claimed: &Value,
    stuck: Option<&str>,
) -> (StatusCode, Value) {
    let path = format!("/builds/{}/release", claimed["build"]);
    let body = json!({ "claim": claimed["claim"], "stuck": stuck });
    call(router, who, "POST", &path, body).await
}

fn shared(labeled: Vec<Value>) -> Shared {
    Arc::new(Mutex::new(Hub {
        labeled,
        ..Hub::default()
    }))
}

/// `who` calls `/builds/{build}/{what}` with the build's claim and `body`'s fields.
async fn on_build(
    router: &Router,
    who: &str,
    claimed: &Value,
    what: &str,
    body: Value,
) -> (StatusCode, Value) {
    let path = format!("/builds/{}/{what}", claimed["build"]);
    let mut body = body;
    body["claim"] = claimed["claim"].clone();
    call(router, who, "POST", &path, body).await
}

async fn mint(router: &Router, who: &str, claimed: &Value) -> (StatusCode, Value) {
    on_build(router, who, claimed, "token", json!({})).await
}

async fn open_pr(router: &Router, who: &str, claimed: &Value) -> (StatusCode, Value) {
    let body = json!({ "title": "Add the thing", "body": "It adds the thing.\n" });
    on_build(router, who, claimed, "pr", body).await
}

async fn report(
    router: &Router,
    who: &str,
    claimed: &Value,
    standing: &str,
) -> (StatusCode, Value) {
    on_build(
        router,
        who,
        claimed,
        "report",
        json!({ "standing": standing }),
    )
    .await
}

async fn age_claims(pool: &PgPool) {
    sqlx::query("UPDATE builds SET heartbeat_at = now() - interval '31 minutes'")
        .execute(pool)
        .await
        .unwrap();
}

#[sqlx::test(migrations = false)]
async fn with_the_lane_off_every_lane_route_refuses(pool: PgPool) {
    let hub = shared(vec![issue(1, &[])]);
    let (router, _) = lane(pool, &hub).await;
    let (status, settings) = call(&router, "ana", "GET", "/settings/builds", Value::Null).await;
    assert_eq!(
        (status, settings),
        (StatusCode::OK, json!({ "enabled": false }))
    );

    let fake = json!({ "build": 1, "claim": 1 });
    for (status, body) in [
        call(&router, "ana", "POST", "/builds/claim", Value::Null).await,
        call(&router, "ana", "GET", "/builds", Value::Null).await,
        call(
            &router,
            "ana",
            "POST",
            "/builds/1/heartbeat",
            json!({ "claim": 1 }),
        )
        .await,
        release(&router, "ana", &fake, None).await,
        mint(&router, "ana", &fake).await,
        open_pr(&router, "ana", &fake).await,
        report(&router, "ana", &fake, "merged").await,
    ] {
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body.as_str().unwrap().contains("build lane is off"),
            "{body}"
        );
    }
    assert!(hub.lock().unwrap().writes.is_empty());
}

#[sqlx::test(migrations = false)]
async fn a_claim_gives_what_a_daemon_needs_and_assigns_the_developer(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    hub.lock().unwrap().config = Some("verify:\n  - cargo test\n".into());
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;

    let claimed = claim(&router, "ana").await;
    assert_eq!(
        claimed,
        json!({
            "build": claimed["build"], "claim": 1, "repo": "o/r",
            "clone_url": "https://github.com/o/r.git", "issue": 7, "title": "Issue 7",
            "body": "Build it.", "default_branch": "trunk", "branch": "mergeloop/build-7",
            "verify": ["cargo test"],
            "author": { "name": "mp[bot]", "email": "900+mp[bot]@users.noreply.github.com" },
            "co_author": { "name": "ana", "email": "1+ana@users.noreply.github.com" },
            "pr": null,
        })
    );
    assert_eq!(
        hub.lock().unwrap().writes,
        [(
            "POST".into(),
            "/repos/o/r/issues/7/assignees".into(),
            r#"{"assignees":["ana"]}"#.into()
        )]
    );
    assert_eq!(
        claim(&router, "ben").await,
        Value::Null,
        "one task, one claim"
    );
}

#[sqlx::test(migrations = false)]
async fn a_task_in_a_repo_the_caller_may_not_push_to_is_never_theirs(pool: PgPool) {
    let hub = shared(vec![issue(1, &[]), issue(2, &["cy"])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;

    assert_eq!(
        claim(&router, "cy").await,
        Value::Null,
        "even the one assigned to him"
    );
    assert_eq!(claim(&router, "ana").await["issue"], 1);
}

#[sqlx::test(migrations = false)]
async fn a_task_assigned_to_ana_goes_to_ana_and_never_to_ben(pool: PgPool) {
    let hub = shared(vec![issue(1, &[]), issue(2, &["ana"])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;

    let ben = claim(&router, "ben").await;
    assert_eq!(ben["issue"], 1, "Ana's is not Ben's");
    assert_eq!(claim(&router, "ben").await, Value::Null);
    let ana = claim(&router, "ana").await;
    assert_eq!(ana["issue"], 2);
    let writes = hub.lock().unwrap().writes.clone();
    assert_eq!(writes.len(), 1, "Ana is already assigned: {writes:?}");
}

#[sqlx::test(migrations = false)]
async fn a_stale_claim_is_claimed_again_and_its_old_holder_refused(pool: PgPool) {
    let hub = shared(vec![issue(1, &[])]);
    let (router, state) = lane(pool.clone(), &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    assert_eq!(heartbeat(&router, "ana", &ana).await, StatusCode::OK);
    assert_eq!(
        heartbeat(&router, "ben", &ana).await,
        StatusCode::CONFLICT,
        "Ben holds nothing"
    );
    assert_eq!(
        claim(&router, "ben").await,
        Value::Null,
        "Ana's claim is fresh"
    );

    sqlx::query("UPDATE builds SET heartbeat_at = now() - interval '31 minutes'")
        .execute(&pool)
        .await
        .unwrap();
    let ben = claim(&router, "ben").await;
    assert_eq!(
        (ben["build"].clone(), ben["claim"].clone()),
        (ana["build"].clone(), json!(2))
    );
    assert_eq!(heartbeat(&router, "ana", &ana).await, StatusCode::CONFLICT);
    let (status, body) = release(&router, "ana", &ana, Some("late")).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.as_str().unwrap().contains("no longer hold"), "{body}");
    assert_eq!(heartbeat(&router, "ben", &ben).await, StatusCode::OK);

    let writes = hub.lock().unwrap().writes.clone();
    let assigned: Vec<(&str, &str)> = writes
        .iter()
        .map(|(m, _, b)| (m.as_str(), b.as_str()))
        .collect();
    assert_eq!(
        assigned,
        [
            ("POST", r#"{"assignees":["ana"]}"#),
            ("POST", r#"{"assignees":["ben"]}"#),
            ("DELETE", r#"{"assignees":["ana"]}"#),
        ]
    );
    let builds = state.builds.builds(1).await.unwrap();
    assert_eq!(builds[0].holder.as_deref(), Some("ben"));
}

#[sqlx::test(migrations = false)]
async fn stuck_comments_on_the_issue_and_takes_the_task_out_of_the_pool(pool: PgPool) {
    let hub = shared(vec![issue(3, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;

    let (status, _) = release(&router, "ana", &ana, Some("  ")).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "a reason says why");
    let (status, build) = release(&router, "ana", &ana, Some("the tests need a GPU")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        (build["status"].as_str(), build["reason"].as_str()),
        (Some("stuck"), Some("the tests need a GPU"))
    );

    let writes = hub.lock().unwrap().writes.clone();
    let (method, path, body) = writes.last().unwrap();
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("POST", "/repos/o/r/issues/3/comments")
    );
    assert!(body.contains("the tests need a GPU"), "{body}");
    assert_eq!(claim(&router, "ben").await, Value::Null);
    assert_eq!(claim(&router, "ana").await, Value::Null);
}

#[sqlx::test(migrations = false)]
async fn a_task_given_back_is_a_task_again_without_the_claims_assignee(pool: PgPool) {
    let hub = shared(vec![issue(3, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    let (status, build) = release(&router, "ana", &ana, None).await;
    assert_eq!(
        (status, build["status"].as_str()),
        (StatusCode::OK, Some("ready"))
    );
    let writes = hub.lock().unwrap().writes.clone();
    assert_eq!(writes.last().unwrap().0, "DELETE");
    assert_eq!(claim(&router, "ben").await["issue"], 3);
}

#[sqlx::test(migrations = false)]
async fn builds_list_for_readers_and_for_their_holder(pool: PgPool) {
    let hub = shared(vec![issue(1, &[]), issue(2, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    assert_eq!(heartbeat(&router, "ana", &ana).await, StatusCode::OK);

    let (status, all) = call(&router, "cy", "GET", "/builds", Value::Null).await;
    assert_eq!(status, StatusCode::OK);
    let all = all.as_array().unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0]["holder"], "ana");
    assert_eq!(all[0]["stage"], "reviewing");
    assert_eq!(all[0]["heard_secs"], 0);
    assert_eq!(all[1]["status"], "ready");

    let (_, mine) = call(&router, "ana", "GET", "/builds?mine=true", Value::Null).await;
    assert_eq!(mine.as_array().unwrap().len(), 1);
    let (_, bens) = call(&router, "cy", "GET", "/builds?mine=true", Value::Null).await;
    assert_eq!(bens, json!([]));
}

async fn deliver(router: &Router, action: &str, issue: Value) -> StatusCode {
    let body = json!({
        "action": action, "issue": issue, "label": { "name": "mergeloop" },
        "installation": { "id": 1 }, "repository": { "full_name": "o/r" },
    })
    .to_string();
    let req = Request::post("/webhooks/github")
        .header("x-github-event", "issues")
        .header(
            "x-hub-signature-256",
            webhook::sign(SECRET, body.as_bytes()),
        )
        .body(Body::from(body))
        .unwrap();
    send(router, req).await.status()
}

async fn listed(router: &Router) -> Vec<(u64, String)> {
    let (status, builds) = call(router, "ana", "GET", "/builds", Value::Null).await;
    assert_eq!(status, StatusCode::OK, "{builds}");
    builds
        .as_array()
        .unwrap()
        .iter()
        .map(|b| {
            (
                b["issue"].as_u64().unwrap(),
                b["title"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[sqlx::test(migrations = false)]
async fn issues_webhooks_create_change_and_remove_tasks(pool: PgPool) {
    let hub = shared(Vec::new());
    let (router, _) = lane(pool, &hub).await;
    // While the lane is off, nothing is kept.
    assert_eq!(
        deliver(&router, "labeled", issue(5, &[])).await,
        StatusCode::NO_CONTENT
    );
    turn_on(&router).await;
    // Reads the labeled issues, none, so the next claims within 5 minutes read none.
    assert_eq!(claim(&router, "ana").await, Value::Null);
    assert!(listed(&router).await.is_empty());

    assert_eq!(
        deliver(&router, "labeled", issue(5, &[])).await,
        StatusCode::NO_CONTENT
    );
    let mut unlabeled = issue(6, &[]);
    unlabeled["labels"] = json!([{ "name": "bug" }]);
    deliver(&router, "opened", unlabeled).await;
    let mut edited = issue(5, &[]);
    edited["title"] = "Better title".into();
    deliver(&router, "edited", edited).await;
    assert_eq!(listed(&router).await, [(5, "Better title".into())]);

    let mut closed = issue(5, &[]);
    closed["state"] = "closed".into();
    deliver(&router, "closed", closed).await;
    assert!(listed(&router).await.is_empty());
    deliver(&router, "reopened", issue(5, &[])).await;
    assert_eq!(listed(&router).await.len(), 1);
    let mut label_gone = issue(5, &[]);
    label_gone["labels"] = json!([]);
    deliver(&router, "unlabeled", label_gone).await;
    assert!(listed(&router).await.is_empty());

    // Labeled again, and assigned to Ben on GitHub: it is Ben's, not Ana's.
    deliver(&router, "labeled", issue(5, &[])).await;
    deliver(&router, "assigned", issue(5, &["ben"])).await;
    assert_eq!(claim(&router, "ana").await, Value::Null);
    assert_eq!(claim(&router, "ben").await["issue"], 5);
}

#[sqlx::test(migrations = false)]
async fn without_any_webhook_a_claim_finds_a_labeled_issue(pool: PgPool) {
    let hub = shared(vec![issue(9, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    assert!(listed(&router).await.is_empty(), "no delivery came");
    assert_eq!(claim(&router, "ana").await["issue"], 9);
}

#[sqlx::test(migrations = false)]
async fn an_issue_in_a_repo_whose_queue_is_off_is_no_task(pool: PgPool) {
    let hub = shared(Vec::new());
    let (router, state) = lane(pool, &hub).await;
    turn_on(&router).await;
    let repo = RepoName::parse("o/r").unwrap();
    state
        .store
        .set_repo_enabled(1, &repo, Some(false))
        .await
        .unwrap();
    deliver(&router, "labeled", issue(5, &[])).await;
    assert!(listed(&router).await.is_empty());
}

#[sqlx::test(migrations = false)]
async fn only_a_signed_in_member_of_the_workspace_may_claim(pool: PgPool) {
    let hub = shared(vec![issue(1, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;

    let elsewhere = Request::post("/api/w/2/builds/claim")
        .header("cookie", "mp_session=ana")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        send(&router, elsewhere).await.status(),
        StatusCode::NOT_FOUND
    );

    // A GitHub token GitHub takes, of someone who may push: a token reaches single repos.
    let token = Request::post("/api/w/1/builds/claim")
        .header("authorization", "Bearer ghu_ana")
        .body(Body::empty())
        .unwrap();
    let res = send(&router, token).await;
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
    assert!(hub.lock().unwrap().writes.is_empty());
}

#[sqlx::test(migrations = false)]
async fn the_holder_gets_a_token_for_its_one_repo_that_pushes_and_opens_prs(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;

    let (status, token) = mint(&router, "ana", &ana).await;
    assert_eq!(status, StatusCode::OK, "{token}");
    assert_eq!(
        token,
        json!({ "token": "ghs_push", "expires_at": "2026-11-12T13:00:00Z" })
    );
    assert_eq!(
        hub.lock().unwrap().mints,
        [json!({
            "repositories": ["r"],
            "permissions": { "contents": "write", "pull_requests": "write" },
        })]
    );
}

#[sqlx::test(migrations = false)]
async fn no_token_without_a_live_claim_held_by_the_caller_who_may_push(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool.clone(), &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    let refused = |(status, body): (StatusCode, Value), want: StatusCode, says: &str| {
        assert_eq!(status, want, "{body}");
        assert!(body.as_str().unwrap().contains(says), "{body}");
    };

    refused(
        mint(&router, "ben", &ana).await,
        StatusCode::CONFLICT,
        "no longer hold",
    );
    let older = json!({ "build": ana["build"], "claim": 0 });
    refused(
        mint(&router, "ana", &older).await,
        StatusCode::CONFLICT,
        "no longer hold",
    );

    let token = Request::post(format!("/api/w/1/builds/{}/token", ana["build"]))
        .header("authorization", "Bearer ghu_ana")
        .header("content-type", "application/json")
        .body(Body::from(json!({ "claim": ana["claim"] }).to_string()))
        .unwrap();
    assert_eq!(
        send(&router, token).await.status(),
        StatusCode::FORBIDDEN,
        "a sign-in only"
    );

    hub.lock().unwrap().revoked.push("ghu_ana".into());
    refused(
        mint(&router, "ana", &ana).await,
        StatusCode::FORBIDDEN,
        "may not push",
    );
    hub.lock().unwrap().revoked.clear();

    age_claims(&pool).await;
    refused(
        mint(&router, "ana", &ana).await,
        StatusCode::CONFLICT,
        "heartbeat",
    );
    assert_eq!(heartbeat(&router, "ana", &ana).await, StatusCode::OK);

    let (status, _) = release(&router, "ana", &ana, None).await;
    assert_eq!(status, StatusCode::OK);
    refused(
        mint(&router, "ana", &ana).await,
        StatusCode::CONFLICT,
        "no longer hold",
    );

    let ana = claim(&router, "ana").await;
    call(
        &router,
        "ana",
        "PUT",
        "/settings/builds",
        json!({ "enabled": false }),
    )
    .await;
    refused(
        mint(&router, "ana", &ana).await,
        StatusCode::CONFLICT,
        "build lane is off",
    );
    assert!(hub.lock().unwrap().mints.is_empty(), "no token was minted");
}

#[sqlx::test(migrations = false)]
async fn the_app_opens_the_pr_ready_labeled_assigned_and_closing_the_issue(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;

    let (status, build) = open_pr(&router, "ana", &ana).await;
    assert_eq!(status, StatusCode::OK, "{build}");
    assert_eq!(
        (build["status"].as_str(), build["pr"].as_u64()),
        (Some("landing"), Some(40))
    );

    let hub = hub.lock().unwrap();
    assert_eq!(
        hub.opened,
        [(
            fake::INSTALLATION_TOKEN.to_owned(),
            json!({
                "head": "mergeloop/build-7", "base": "trunk", "title": "Add the thing",
                "body": "Closes #7\n\nIt adds the thing.", "draft": false,
            })
        )]
    );
    let pr_writes: Vec<(&str, &str)> = hub
        .writes
        .iter()
        .filter(|(_, path, _)| path.starts_with("/repos/o/r/issues/40/"))
        .map(|(_, path, body)| (path.as_str(), body.as_str()))
        .collect();
    assert_eq!(
        pr_writes,
        [
            ("/repos/o/r/issues/40/labels", r#"{"labels":["mergeloop"]}"#),
            ("/repos/o/r/issues/40/assignees", r#"{"assignees":["ana"]}"#),
        ]
    );
}

#[sqlx::test(migrations = false)]
async fn opening_twice_or_after_a_lost_answer_opens_one_pr(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    // The PR was opened, but its answer never came back.
    hub.lock()
        .unwrap()
        .prs
        .push(Pr::bots(40, "mergeloop/build-7"));

    let (_, first) = open_pr(&router, "ana", &ana).await;
    let (status, second) = open_pr(&router, "ana", &ana).await;
    assert_eq!(status, StatusCode::OK, "{second}");
    assert_eq!(
        (first["pr"].as_u64(), second["pr"].as_u64()),
        (Some(40), Some(40))
    );
    let hub = hub.lock().unwrap();
    assert!(hub.opened.is_empty(), "{:?}", hub.opened);
    assert_eq!(hub.prs.len(), 1);
}

#[sqlx::test(migrations = false)]
async fn only_the_holder_opens_the_pr_with_a_title(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;

    assert_eq!(open_pr(&router, "ben", &ana).await.0, StatusCode::CONFLICT);
    let blank = on_build(
        &router,
        "ana",
        &ana,
        "pr",
        json!({ "title": " ", "body": "" }),
    )
    .await;
    assert_eq!(blank.0, StatusCode::BAD_REQUEST);
    assert!(hub.lock().unwrap().opened.is_empty());
}

#[sqlx::test(migrations = false)]
async fn the_holder_reports_waiting_then_merged_which_ends_the_build(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;

    let (status, body) = report(&router, "ana", &ana, "merged").await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.as_str().unwrap().contains("no PR"), "{body}");

    open_pr(&router, "ana", &ana).await;
    let (status, waiting) = report(&router, "ana", &ana, "waiting").await;
    assert_eq!(
        (status, waiting["status"].as_str()),
        (StatusCode::OK, Some("waiting"))
    );
    let (_, mine) = call(&router, "ana", "GET", "/builds?mine=true", Value::Null).await;
    assert_eq!(
        mine.as_array().unwrap().len(),
        1,
        "a waiting build is still Ana's"
    );
    let (_, landing) = report(&router, "ana", &ana, "landing").await;
    assert_eq!(landing["status"], "landing");
    hub.lock().unwrap().prs[0].merged = true;
    let (_, merged) = report(&router, "ana", &ana, "merged").await;
    assert_eq!(
        (merged["status"].as_str(), merged["pr"].as_u64()),
        (Some("merged"), Some(40))
    );

    assert_eq!(
        heartbeat(&router, "ana", &ana).await,
        StatusCode::CONFLICT,
        "it is over"
    );
    assert_eq!(mint(&router, "ana", &ana).await.0, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = false)]
async fn a_build_with_a_pr_survives_its_issue_closing_and_the_lane_off_and_on(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    open_pr(&router, "ana", &ana).await;

    let mut closed = issue(7, &[]);
    closed["state"] = "closed".into();
    deliver(&router, "closed", closed).await;
    let mut unlabeled = issue(7, &[]);
    unlabeled["labels"] = json!([]);
    deliver(&router, "unlabeled", unlabeled).await;
    call(
        &router,
        "ana",
        "PUT",
        "/settings/builds",
        json!({ "enabled": false }),
    )
    .await;
    turn_on(&router).await;
    hub.lock().unwrap().labeled.clear();
    assert_eq!(
        claim(&router, "ben").await,
        Value::Null,
        "a sync keeps it too"
    );

    assert_eq!(heartbeat(&router, "ana", &ana).await, StatusCode::OK);
    let (_, builds) = call(&router, "ana", "GET", "/builds", Value::Null).await;
    assert_eq!(builds[0]["status"], "landing");
    assert_eq!(builds[0]["pr"], 40);
}

#[sqlx::test(migrations = false)]
async fn a_stale_build_with_a_pr_is_claimed_with_its_pr_and_fences_out_the_old_holder(
    pool: PgPool,
) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool.clone(), &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    open_pr(&router, "ana", &ana).await;
    age_claims(&pool).await;

    let ben = claim(&router, "ben").await;
    assert_eq!(
        (ben["build"].clone(), ben["pr"].as_u64()),
        (ana["build"].clone(), Some(40))
    );
    assert_eq!(mint(&router, "ana", &ana).await.0, StatusCode::CONFLICT);
    assert_eq!(
        report(&router, "ana", &ana, "merged").await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(mint(&router, "ben", &ben).await.0, StatusCode::OK);
    let (_, again) = open_pr(&router, "ben", &ben).await;
    assert_eq!(again["pr"], 40, "Ben's call finds Ana's PR");
    assert_eq!(hub.lock().unwrap().opened.len(), 1);
}

#[sqlx::test(migrations = false)]
async fn an_issue_closed_while_its_build_is_claimed_cancels_the_build_until_given_back(
    pool: PgPool,
) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    let mut closed = issue(7, &[]);
    closed["state"] = "closed".into();
    deliver(&router, "closed", closed).await;

    let (status, body) = on_build(&router, "ana", &ana, "heartbeat", json!({})).await;
    assert_eq!(status, StatusCode::CONFLICT);
    assert!(body.as_str().unwrap().contains("closed"), "{body}");
    assert_eq!(mint(&router, "ana", &ana).await.0, StatusCode::CONFLICT);
}

fn ana_holds(claimed: &Value) -> (u64, u64) {
    (
        claimed["build"].as_u64().unwrap(),
        claimed["claim"].as_u64().unwrap(),
    )
}

/// As if every repo's labeled issues were last read over 5 minutes ago.
async fn sync_due(pool: &PgPool) {
    sqlx::query("UPDATE build_syncs SET synced_at = now() - interval '6 minutes'")
        .execute(pool)
        .await
        .unwrap();
}

fn closed(number: u64) -> Value {
    let mut closed = issue(number, &[]);
    closed["state"] = "closed".into();
    closed
}

#[sqlx::test(migrations = false)]
async fn a_same_login_takeover_still_gives_back_the_lanes_assignment(pool: PgPool) {
    let hub = shared(vec![issue(1, &[])]);
    let (router, _) = lane(pool.clone(), &hub).await;
    turn_on(&router).await;
    claim(&router, "ana").await;
    deliver(&router, "assigned", issue(1, &["ana"])).await;
    age_claims(&pool).await;

    let again = claim(&router, "ana").await;
    let (status, _) = release(&router, "ana", &again, None).await;
    assert_eq!(status, StatusCode::OK);
    let writes = hub.lock().unwrap().writes.clone();
    assert_eq!(
        writes
            .last()
            .map(|(m, p, b)| (m.as_str(), p.as_str(), b.as_str())),
        Some((
            "DELETE",
            "/repos/o/r/issues/1/assignees",
            r#"{"assignees":["ana"]}"#
        )),
        "{writes:?}"
    );
    assert_eq!(claim(&router, "ben").await["issue"], 1);
}

#[sqlx::test(migrations = false)]
async fn a_held_build_whose_issue_left_the_label_never_returns_to_the_pool(pool: PgPool) {
    let hub = shared(vec![issue(1, &[]), issue(2, &[])]);
    let (router, _) = lane(pool.clone(), &hub).await;
    turn_on(&router).await;
    let one = claim(&router, "ana").await;
    let two = claim(&router, "ana").await;

    // #1 leaves the label by webhook; #2 by a sync that no longer lists it.
    let mut unlabeled = issue(1, &[]);
    unlabeled["labels"] = json!([]);
    deliver(&router, "unlabeled", unlabeled).await;
    hub.lock().unwrap().labeled.clear();
    sync_due(&pool).await;
    assert_eq!(claim(&router, "ben").await, Value::Null);

    for held in [&one, &two] {
        let (status, body) = heartbeat_says(&router, "ana", held).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("release_build"), "{body}");
        let (status, back) = release(&router, "ana", held, None).await;
        assert_eq!(status, StatusCode::OK, "{back}");
    }
    assert_eq!(
        claim(&router, "ben").await,
        Value::Null,
        "neither is a task"
    );
    age_claims(&pool).await;
    assert_eq!(claim(&router, "ben").await, Value::Null);
    assert!(listed(&router).await.is_empty());
}

async fn heartbeat_says(router: &Router, who: &str, claimed: &Value) -> (StatusCode, String) {
    let (status, body) = on_build(router, who, claimed, "heartbeat", json!({})).await;
    (
        status,
        body.as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| body.to_string()),
    )
}

#[sqlx::test(migrations = false)]
async fn a_sync_read_before_a_close_webhook_does_not_bring_the_task_back(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool.clone(), &hub).await;
    turn_on(&router).await;
    deliver(&router, "labeled", issue(7, &[])).await;
    let repo = RepoName::parse("o/r").unwrap();
    // The issue closes while the sync's list is on its way back.
    hook(
        &hub,
        &pool,
        "/repos/o/r/issues?labels",
        move |store| async move {
            store.drop_task(1, &repo, 7).await.unwrap();
        },
    );

    assert_eq!(claim(&router, "ana").await, Value::Null);
    assert!(listed(&router).await.is_empty());
}

#[sqlx::test(migrations = false)]
async fn a_sync_read_before_a_label_webhook_does_not_drop_the_new_task(pool: PgPool) {
    let hub = shared(Vec::new());
    let (router, _) = lane(pool.clone(), &hub).await;
    turn_on(&router).await;
    let repo = RepoName::parse("o/r").unwrap();
    hook(
        &hub,
        &pool,
        "/repos/o/r/issues?labels",
        move |store| async move {
            let task = mergepatrol_store::IssueTask {
                number: 8,
                title: "Issue 8".into(),
                body: String::new(),
                assignees: Vec::new(),
            };
            store.upsert_task(1, &repo, &task).await.unwrap();
        },
    );

    assert_eq!(claim(&router, "ana").await["issue"], 8);
}

/// Ana marks #7 stuck, after opening a PR for it when `pr`.
async fn stuck(router: &Router, pr: bool) -> Value {
    turn_on(router).await;
    let ana = claim(router, "ana").await;
    if pr {
        assert_eq!(open_pr(router, "ana", &ana).await.0, StatusCode::OK);
    }
    let (status, _) = release(router, "ana", &ana, Some("ejected twice")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(claim(router, "ben").await, Value::Null);
    ana
}

#[sqlx::test(migrations = false)]
async fn a_stuck_build_is_a_task_again_once_the_label_is_put_back(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    stuck(&router, false).await;
    // Its removal was never heard.
    deliver(&router, "labeled", issue(7, &[])).await;
    let ben = claim(&router, "ben").await;
    assert_eq!((ben["issue"].as_u64(), ben["pr"].as_u64()), (Some(7), None));
}

#[sqlx::test(migrations = false)]
async fn a_stuck_build_with_a_pr_comes_back_with_its_pr(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    let ana = stuck(&router, true).await;
    deliver(&router, "labeled", issue(7, &[])).await;
    let ben = claim(&router, "ben").await;
    assert_eq!(
        (ben["issue"].as_u64(), ben["pr"].as_u64()),
        (Some(7), Some(40))
    );
    assert_eq!(heartbeat(&router, "ana", &ana).await, StatusCode::CONFLICT);
}

#[sqlx::test(migrations = false)]
async fn without_webhooks_a_stuck_build_comes_back_once_its_events_show_the_label_put_back(
    pool: PgPool,
) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool.clone(), &hub).await;
    stuck(&router, false).await;
    let labeled = |at: &str, name: &str| {
        (
            7,
            json!({ "event": "labeled", "created_at": at, "label": { "name": name } }),
        )
    };
    hub.lock().unwrap().events = vec![
        labeled("2020-01-01T00:00:00Z", "mergeloop"),
        labeled("2999-01-01T00:00:00Z", "bug"),
    ];
    sync_due(&pool).await;
    assert_eq!(
        claim(&router, "ben").await,
        Value::Null,
        "labeled before it got stuck"
    );
    assert!(
        hub.lock()
            .unwrap()
            .reads
            .iter()
            .any(|p| p.starts_with("/repos/o/r/issues/7/events")),
        "the sync read its events"
    );

    hub.lock()
        .unwrap()
        .events
        .push(labeled("2999-01-01T00:00:00Z", "MergeLoop"));
    sync_due(&pool).await;
    assert_eq!(claim(&router, "ben").await["issue"], 7);
}

#[sqlx::test(migrations = false)]
async fn a_sync_reads_a_bounded_number_of_pages_and_drops_nothing_it_did_not_see(pool: PgPool) {
    let hub = shared((1..=5000).map(|n| issue(n, &["cy"])).collect());
    let (router, state) = lane(pool.clone(), &hub).await;
    turn_on(&router).await;
    // #5000 is a task the sync will not reach.
    deliver(&router, "labeled", issue(5000, &[])).await;

    assert_eq!(claim(&router, "ana").await["issue"], 5000);
    let pages = hub
        .lock()
        .unwrap()
        .reads
        .iter()
        .filter(|p| p.starts_with("/repos/o/r/issues?labels"))
        .count();
    assert!(pages <= 10, "{pages} pages");
    let builds = state.builds.builds(1).await.unwrap();
    assert_eq!(builds.len(), 1001, "the pages read, and #5000");
}

#[sqlx::test(migrations = false)]
async fn a_token_minted_while_the_claim_is_given_back_is_revoked_and_refused(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool.clone(), &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    let (build, claim_n) = ana_holds(&ana);
    hook(
        &hub,
        &pool,
        "/app/installations/1/access_tokens",
        move |store| async move {
            let ana = mergepatrol_store::Holder {
                installation_id: 1,
                build,
                login: "ana",
                claim: claim_n,
            };
            store.release_build(ana, None).await.unwrap().unwrap();
        },
    );

    let (status, body) = mint(&router, "ana", &ana).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.as_str().unwrap().contains("no longer hold"), "{body}");
    assert_eq!(hub.lock().unwrap().revoked_tokens, ["ghs_push"]);
}

#[sqlx::test(migrations = false)]
async fn a_token_minted_while_the_lane_is_turned_off_is_revoked_and_refused(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool.clone(), &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    hook(
        &hub,
        &pool,
        "/app/installations/1/access_tokens",
        |store| async move {
            let off = mergepatrol_store::BuildSettings { enabled: false };
            store.set_build_settings(1, &off).await.unwrap();
        },
    );

    let (status, body) = mint(&router, "ana", &ana).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body.as_str().unwrap().contains("build lane is off"),
        "{body}"
    );
    assert_eq!(hub.lock().unwrap().revoked_tokens, ["ghs_push"]);
}

#[sqlx::test(migrations = false)]
async fn giving_back_a_build_with_a_pr_ends_the_old_holders_claim(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    open_pr(&router, "ana", &ana).await;
    let (status, _) = release(&router, "ana", &ana, None).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(heartbeat(&router, "ana", &ana).await, StatusCode::CONFLICT);
    assert_eq!(
        report(&router, "ana", &ana, "waiting").await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(mint(&router, "ana", &ana).await.0, StatusCode::CONFLICT);
    assert_eq!(open_pr(&router, "ana", &ana).await.0, StatusCode::CONFLICT);
    let again = claim(&router, "ana").await;
    assert_eq!(again["pr"], 40);
    assert_eq!(heartbeat(&router, "ana", &again).await, StatusCode::OK);
}

#[sqlx::test(migrations = false)]
async fn opening_the_pr_asks_github_afresh_whether_the_caller_may_push(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    hub.lock().unwrap().revoked.push("ghu_ana".into());

    let (status, body) = open_pr(&router, "ana", &ana).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(hub.lock().unwrap().opened.is_empty());
}

#[sqlx::test(migrations = false)]
async fn a_pr_whose_label_fails_is_kept_and_labeled_on_the_next_call(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    hub.lock().unwrap().failing = vec!["/repos/o/r/issues/40/labels".into()];

    let (status, body) = open_pr(&router, "ana", &ana).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.as_str().unwrap().contains("label"), "{body}");
    // Its issue closes before the next call: the build has a PR, so it stays.
    deliver(&router, "closed", closed(7)).await;
    let (_, builds) = call(&router, "ana", "GET", "/builds", Value::Null).await;
    assert_eq!(builds[0]["pr"], 40, "{builds}");

    hub.lock().unwrap().failing.clear();
    let (status, build) = open_pr(&router, "ana", &ana).await;
    assert_eq!((status, build["pr"].as_u64()), (StatusCode::OK, Some(40)));
    let hub = hub.lock().unwrap();
    assert_eq!(hub.opened.len(), 1);
    let labels = hub
        .writes
        .iter()
        .filter(|(_, p, _)| p == "/repos/o/r/issues/40/labels")
        .count();
    assert_eq!(labels, 1, "the failed one is not written");
}

#[sqlx::test(migrations = false)]
async fn a_pr_whose_assignment_fails_fails_the_call_until_it_is_assigned(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    hub.lock().unwrap().failing = vec!["/repos/o/r/issues/40/assignees".into()];

    let (status, body) = open_pr(&router, "ana", &ana).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.as_str().unwrap().contains("assign"), "{body}");

    hub.lock().unwrap().failing.clear();
    let (status, _) = open_pr(&router, "ana", &ana).await;
    assert_eq!(status, StatusCode::OK);
    let assigned = hub
        .lock()
        .unwrap()
        .writes
        .iter()
        .any(|(_, p, _)| p == "/repos/o/r/issues/40/assignees");
    assert!(assigned);
}

#[sqlx::test(migrations = false)]
async fn only_the_apps_open_pr_from_the_builds_branch_into_the_default_branch_is_adopted(
    pool: PgPool,
) {
    type Change = fn(&mut Pr);
    let cases: [(&str, Change); 4] = [
        ("into another base", |pr| pr.base = "release".into()),
        ("opened by someone else", |pr| pr.author = "ana".into()),
        ("closed", |pr| pr.open = false),
        ("from a fork", |pr| pr.head_repo = "ana/r".into()),
    ];
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    for (why, change) in cases {
        let mut pr = Pr::bots(40, "mergeloop/build-7");
        change(&mut pr);
        {
            let mut h = hub.lock().unwrap();
            h.prs = vec![pr];
            // GitHub refuses a new one, as it would for the open one, or for a branch not pushed.
            h.failing = vec!["/repos/o/r/pulls".into()];
        }

        let (status, body) = open_pr(&router, "ana", &ana).await;
        assert_eq!(status, StatusCode::CONFLICT, "{why}: {body}");
        assert!(body.as_str().unwrap().contains("#40"), "{why}: {body}");
    }
    let (_, builds) = call(&router, "ana", "GET", "/builds", Value::Null).await;
    assert_eq!(builds[0]["pr"], Value::Null);
}

#[sqlx::test(migrations = false)]
async fn merged_is_taken_only_when_github_says_the_pr_merged(pool: PgPool) {
    let hub = shared(vec![issue(7, &[])]);
    let (router, _) = lane(pool, &hub).await;
    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    open_pr(&router, "ana", &ana).await;

    let (status, body) = report(&router, "ana", &ana, "merged").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(body.as_str().unwrap().contains("not merged"), "{body}");
    hub.lock().unwrap().prs[0].merged = true;
    let (status, merged) = report(&router, "ana", &ana, "merged").await;
    assert_eq!(
        (status, merged["status"].as_str()),
        (StatusCode::OK, Some("merged"))
    );
}

/// `who` calls `tool` through `/mcp`, signed in: the tool's result.
async fn mcp_tool(router: &Router, who: &str, tool: &str, args: Value) -> Value {
    let message = json!({
        "jsonrpc": "2.0", "id": 1, "method": "tools/call",
        "params": { "name": tool, "arguments": args },
    });
    let req = Request::post("/mcp")
        .header("authorization", format!("Bearer {who}"))
        .header("content-type", "application/json")
        .body(Body::from(message.to_string()))
        .unwrap();
    let res = send(router, req).await;
    assert_eq!(res.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
        .await
        .unwrap();
    serde_json::from_slice::<Value>(&bytes).unwrap()["result"].clone()
}

/// What kind of refusal a tool's result says it is: its route's status, and the refusal's name
/// when the route named one.
fn refusal(result: &Value) -> (Value, Value) {
    assert_eq!(result["isError"], true, "{result}");
    (
        result["_meta"]["status"].clone(),
        result["_meta"]["refusal"].clone(),
    )
}

#[sqlx::test(migrations = false)]
async fn mcp_says_which_refusal_a_daemon_must_act_on(pool: PgPool) {
    let hub = shared(vec![issue(7, &[]), issue(8, &[])]);
    let (router, _) = lane(pool.clone(), &hub).await;
    let on = |claimed: &Value| json!({ "workspace": 1, "build": claimed["build"], "claim": claimed["claim"] });

    let off = mcp_tool(&router, "ana", "claim_build", json!({ "workspace": 1 })).await;
    assert_eq!(refusal(&off), (json!(409), json!("lane_off")));

    turn_on(&router).await;
    let ana = claim(&router, "ana").await;
    let ok = mcp_tool(&router, "ana", "heartbeat_build", on(&ana)).await;
    assert_eq!(ok["isError"], false, "{ok}");
    assert!(ok.get("_meta").is_none(), "{ok}");

    hub.lock().unwrap().revoked.push("ghu_ana".into());
    let barred = mcp_tool(&router, "ana", "mint_push_token", on(&ana)).await;
    assert_eq!(refusal(&barred), (json!(403), json!("may_not_push")));
    hub.lock().unwrap().revoked.clear();

    age_claims(&pool).await;
    let unheard = mcp_tool(&router, "ana", "mint_push_token", on(&ana)).await;
    assert_eq!(refusal(&unheard), (json!(409), json!("unheard")));

    let lost = mcp_tool(&router, "ben", "heartbeat_build", on(&ana)).await;
    assert_eq!(refusal(&lost), (json!(409), json!("lost")));

    let ana = claim(&router, "ana").await;
    let mut closed = issue(ana["issue"].as_u64().unwrap(), &[]);
    closed["state"] = "closed".into();
    deliver(&router, "closed", closed).await;
    let cancelled = mcp_tool(&router, "ana", "heartbeat_build", on(&ana)).await;
    assert_eq!(refusal(&cancelled), (json!(409), json!("cancelled")));

    let blank = mcp_tool(
        &router,
        "ana",
        "release_build",
        json!({ "workspace": 1, "build": ana["build"], "claim": ana["claim"], "stuck": " " }),
    )
    .await;
    assert_eq!(refusal(&blank), (json!(400), Value::Null));
}
