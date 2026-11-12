//! The build lane's routes, with GitHub faked: Ana and Ben may push to `o/r`, Cy may not.

use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::*;

const SECRET: &str = "whs";

/// What the fake GitHub has, and what it was asked to change.
#[derive(Default)]
struct Hub {
    /// `o/r`'s open issues with the `mergeloop` label, as the issues API lists them.
    labeled: Vec<Value>,
    /// `o/r`'s config file.
    config: Option<String>,
    /// The writes: method, path, body.
    writes: Vec<(String, String, String)>,
}

type Shared = Arc<Mutex<Hub>>;

fn issue(number: u64, assignees: &[&str]) -> Value {
    json!({
        "number": number, "title": format!("Issue {number}"), "body": "Build it.",
        "state": "open", "labels": [{ "name": "mergeloop" }],
        "assignees": assignees.iter().map(|a| json!({ "login": a })).collect::<Vec<_>>(),
    })
}

async fn github(hub: Shared) -> GitHub {
    fake::github(move |call| {
        let mut hub = hub.lock().unwrap();
        let push = |yes: bool| (200, json!({ "permissions": { "push": yes } }).to_string());
        match (call.method, call.path) {
            ("GET", "/rate_limit") => (200, "{}".into()),
            ("GET", "/repos/o/r") => push(matches!(call.token, "ghu_ana" | "ghu_ben")),
            ("GET", "/repos/o/y") => push(true),
            ("POST", "/graphql") => {
                let file = hub.config.as_ref().map(|text| json!({ "object": { "text": text } }));
                let target = json!({ "oid": "abc", "c0": file, "c1": null });
                let data = json!({ "repository": { "defaultBranchRef": { "name": "trunk", "target": target } } });
                (200, json!({ "data": data }).to_string())
            }
            ("GET", "/repos/o/r/issues?labels=mergeloop&state=open&per_page=100&page=1") => {
                (200, Value::from(hub.labeled.clone()).to_string())
            }
            ("GET", "/repos/o/y/issues?labels=mergeloop&state=open&per_page=100&page=1") => {
                (200, "[]".into())
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
        "action": action, "issue": issue,
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
