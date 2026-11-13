//! An MCP client's sign-in, through GitHub (faked) and the consent page: no code reaches the
//! client until the user says yes on the page, in the browser that signed in.

use axum::http::HeaderMap;

use super::*;

const REDIRECT: &str = "http%3A%2F%2F127.0.0.1%3A9999%2Fcallback";

/// GitHub, as far as a sign-in reaches it: Ana signs in, and can access installation 1.
async fn github() -> GitHub {
    fake::github(|call| match (call.method, call.path) {
        ("POST", "/login/oauth/access_token") => {
            (200, r#"{"access_token":"ghu_ana","token_type":"bearer"}"#.into())
        }
        ("GET", "/user") => (200, r#"{"login":"ana","avatar_url":""}"#.into()),
        ("GET", "/user/installations?per_page=100&page=1") => (
            200,
            r#"{"installations":[{"id":1,"account":{"login":"o","type":"Organization","avatar_url":""}}]}"#.into(),
        ),
        _ => (404, "{}".into()),
    })
    .await
}

/// A client signing in, up to the consent page.
struct SignIn {
    router: Router,
    pkce: Pkce,
    /// Where the callback sent the browser.
    page: String,
    /// The browser's cookies after the callback.
    cookies: String,
}

impl SignIn {
    /// Registers a client called `name`, and signs in with `extra` added to its authorize request.
    async fn start(router: &Router, name: &str, extra: &str) -> SignIn {
        let body = serde_json::json!({ "client_name": name, "redirect_uris": ["http://127.0.0.1:9999/callback"] });
        let req = Request::post("/oauth/register")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let registered = json_of(send(router, req).await).await;
        let client_id = registered["client_id"].as_str().unwrap();
        let pkce = Pkce::new();
        let req = Request::get(format!(
            "/oauth/authorize?response_type=code&client_id={}&redirect_uri={REDIRECT}&state=s%20t&code_challenge={}&code_challenge_method=S256{extra}",
            auth::percent_encode(client_id),
            pkce.challenge
        ))
        .body(Body::empty())
        .unwrap();
        let (to_github, cookies) = redirect(&send(router, req).await);
        assert!(to_github.starts_with("https://github.com/"), "{to_github}");
        let callback = Request::get(format!(
            "/auth/callback?code=gh&state={}",
            query_param(&to_github, "state")
        ))
        .header("cookie", cookies.join("; "))
        .body(Body::empty())
        .unwrap();
        let (page, set) = redirect(&send(router, callback).await);
        SignIn {
            router: router.clone(),
            pkce,
            page,
            cookies: set
                .into_iter()
                .filter(|c| !c.ends_with('='))
                .collect::<Vec<_>>()
                .join("; "),
        }
    }

    fn request(&self) -> String {
        query_param(&self.page, "request")
    }

    /// The consent page, as the browser with `cookies` gets it.
    async fn page_as(&self, cookies: &str) -> (StatusCode, HeaderMap, String) {
        let req = Request::get(&self.page)
            .header("cookie", cookies)
            .body(Body::empty())
            .unwrap();
        let res = send(&self.router, req).await;
        let (status, headers) = (res.status(), res.headers().clone());
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
    }

    /// The page's answer `decision` for `request`, as the browser with `cookies` posts it.
    async fn answer_as(&self, cookies: &str, request: &str, decision: &str) -> Response {
        let req = Request::post("/oauth/consent")
            .header("content-type", "application/x-www-form-urlencoded")
            .header("cookie", cookies)
            .body(Body::from(format!("request={request}&decision={decision}")))
            .unwrap();
        send(&self.router, req).await
    }

    async fn answer(&self, decision: &str) -> Response {
        self.answer_as(&self.cookies, &self.request(), decision)
            .await
    }

    /// Trades the code the client got for its session.
    async fn trade(&self, code: &str) -> serde_json::Value {
        let req = Request::post("/oauth/token")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(format!(
                "grant_type=authorization_code&code={code}&code_verifier={}",
                self.pkce.verifier
            )))
            .unwrap();
        json_of(send(&self.router, req).await).await
    }
}

#[sqlx::test(migrations = false)]
async fn an_mcp_sign_in_gives_its_code_only_after_the_user_allows_it(pool: PgPool) {
    let (router, _) = with_github(pool, github().await).await;
    let sign_in = SignIn::start(&router, "Claude Code", "").await;

    // The callback goes to the page, not the client.
    assert!(
        sign_in.page.starts_with("/oauth/consent?request="),
        "{}",
        sign_in.page
    );
    assert!(
        sign_in.cookies.starts_with("mp_consent="),
        "{}",
        sign_in.cookies
    );
    assert!(!sign_in.page.contains("code="), "{}", sign_in.page);

    let (status, headers, page) = sign_in.page_as(&sign_in.cookies).await;
    assert_eq!(status, StatusCode::OK);
    assert!(page.contains("Claude Code"), "{page}");
    assert!(page.contains("A program on this computer"), "{page}");
    assert!(page.contains("ana"), "{page}");
    assert!(page.contains("acts as you"), "{page}");
    assert!(page.contains("No code was given"), "{page}");
    assert!(
        page.contains(&sign_in.request()),
        "the form carries the request"
    );
    assert_eq!(headers["x-frame-options"], "DENY");
    assert_eq!(headers["referrer-policy"], "no-referrer");
    assert_eq!(headers["cache-control"], "no-store");

    let (back, _) = redirect(&sign_in.answer("allow").await);
    assert!(
        back.starts_with("http://127.0.0.1:9999/callback?code="),
        "{back}"
    );
    assert!(back.ends_with("&state=s%20t"), "{back}");
    let token = sign_in.trade(&query_param(&back, "code")).await;
    let token = token["access_token"].as_str().unwrap().to_owned();
    let req = Request::get("/api/session")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap();
    let session = json_of(send(&router, req).await).await;
    assert_eq!(session["user"]["login"], "ana");
}

#[sqlx::test(migrations = false)]
async fn the_page_shows_the_code_the_sign_in_carries(pool: PgPool) {
    let (router, _) = with_github(pool, github().await).await;
    let sign_in = SignIn::start(&router, "mergeloop login", "&user_code=WXYZ-2345").await;
    let (_, _, page) = sign_in.page_as(&sign_in.cookies).await;
    assert!(page.contains("WXYZ-2345"), "{page}");
    assert!(page.contains("must match"), "{page}");
    assert!(!page.contains("No code was given"), "{page}");

    // A name is the client's to choose: it is shown, never run.
    let sign_in = SignIn::start(&router, "<script>x</script>", "").await;
    let (_, _, page) = sign_in.page_as(&sign_in.cookies).await;
    assert!(!page.contains("<script>x"), "{page}");
    assert!(page.contains("&lt;script&gt;x"), "{page}");

    // A code that is not one, back to the client before GitHub.
    let req = Request::get(format!(
        "/oauth/authorize?response_type=code&client_id=c&redirect_uri={REDIRECT}&state=x&code_challenge={}&code_challenge_method=S256&user_code=%3Cb%3E",
        Pkce::new().challenge
    ))
    .body(Body::empty())
    .unwrap();
    let (back, _) = redirect(&send(&router, req).await);
    assert!(
        back.starts_with("http://127.0.0.1:9999/callback?error=invalid_request"),
        "{back}"
    );
}

#[sqlx::test(migrations = false)]
async fn only_the_browser_that_signed_in_may_answer_and_only_once_in_time(pool: PgPool) {
    let (router, _) = with_github(pool.clone(), github().await).await;
    let sign_in = SignIn::start(&router, "Claude Code", "").await;
    let other = SignIn::start(&router, "Claude Code", "").await;
    let refused = |res: Response| {
        assert_eq!(res.status(), StatusCode::FORBIDDEN);
        assert!(res.headers().get("location").is_none());
    };

    // Another browser, or none: neither the page nor the answer.
    let (status, ..) = sign_in.page_as(&other.cookies).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    refused(
        sign_in
            .answer_as(&other.cookies, &sign_in.request(), "allow")
            .await,
    );
    refused(sign_in.answer_as("", &sign_in.request(), "allow").await);
    // The browser, but a request it was not shown.
    refused(sign_in.answer_as(&sign_in.cookies, "guess", "allow").await);

    // Once only.
    let (back, _) = redirect(&sign_in.answer("allow").await);
    assert!(back.contains("code="), "{back}");
    refused(sign_in.answer("allow").await);
    refused(sign_in.answer("deny").await);

    // Not after its time.
    sqlx::query("UPDATE consents SET expires_at = 0")
        .execute(&pool)
        .await
        .unwrap();
    let (status, ..) = other.page_as(&other.cookies).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    refused(other.answer("allow").await);
}

#[sqlx::test(migrations = false)]
async fn a_denied_sign_in_reaches_the_client_as_an_error(pool: PgPool) {
    let (router, _) = with_github(pool.clone(), github().await).await;
    let sign_in = SignIn::start(&router, "Claude Code", "").await;
    let sessions = || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sessions")
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    let before = sessions().await;

    let (back, _) = redirect(&sign_in.answer("deny").await);
    assert!(
        back.starts_with("http://127.0.0.1:9999/callback?error=access_denied&error_description="),
        "{back}"
    );
    assert!(back.ends_with("&state=s%20t"), "{back}");
    // The session it would have handed over is gone.
    assert_eq!(sessions().await, before - 1);
}
