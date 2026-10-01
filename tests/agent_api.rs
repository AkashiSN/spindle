//! エージェント API（`/api/agent/*`、P5-4a）

mod agent_support;
use agent_support::*;

#[tokio::test]
async fn pair_issues_token_once() {
    let app = App::new().await;
    let id = app.create_iphone("iPhone").await;
    let code = app.pair_code(id).await;
    let (st, v) = app
        .agent_call(
            None,
            Method::POST,
            "/api/agent/pair",
            Some(json!({"code": code})),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert!(v["token"].as_str().unwrap().contains('.'));
    assert_eq!(v["device_name"], "iPhone");
    // 1 回限り
    let (st, _) = app
        .agent_call(
            None,
            Method::POST,
            "/api/agent/pair",
            Some(json!({"code": code})),
        )
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn unknown_selector_is_401_and_not_counted() {
    let app = App::new().await;
    let id = app.create_iphone("iPhone").await;
    let code = app.pair_code(id).await;
    let (_, secret) = code.split_once('.').unwrap();
    for _ in 0..10 {
        let bogus = format!("aaaaaaaaaaaaa.{secret}");
        let (st, _) = app
            .agent_call(
                None,
                Method::POST,
                "/api/agent/pair",
                Some(json!({"code": bogus})),
            )
            .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED);
    }
    let (st, _) = app
        .agent_call(
            None,
            Method::POST,
            "/api/agent/pair",
            Some(json!({"code": code})),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "引けないセレクタは試行に数えない");
}

#[tokio::test]
async fn concurrent_wrong_attempts_stop_at_five() {
    let app = App::new().await;
    let id = app.create_iphone("iPhone").await;
    let code = app.pair_code(id).await;
    let (sel, _) = code.split_once('.').unwrap();
    let wrong = format!("{sel}.{}", "a".repeat(32));
    let futs = (0..10).map(|_| {
        app.agent_call(
            None,
            Method::POST,
            "/api/agent/pair",
            Some(json!({"code": wrong.clone()})),
        )
    });
    let results = futures_util::future::join_all(futs).await;
    assert!(results
        .iter()
        .all(|(st, _)| *st == StatusCode::UNAUTHORIZED));
    let attempts: i64 = app
        .db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT pair_code_attempts FROM devices WHERE id = ?1",
                [id],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert!(attempts <= 5, "{attempts}");
    let (st, _) = app
        .agent_call(
            None,
            Method::POST,
            "/api/agent/pair",
            Some(json!({"code": code})),
        )
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "上限で失効している");
}

#[tokio::test]
async fn expired_code_is_rejected() {
    let app = App::new().await;
    let id = app.create_iphone("iPhone").await;
    let code = app.pair_code(id).await;
    app.db
        .write(move |c| {
            c.execute(
                "UPDATE devices SET pair_code_expires = 1 WHERE id = ?1",
                [id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (st, _) = app
        .agent_call(
            None,
            Method::POST,
            "/api/agent/pair",
            Some(json!({"code": code})),
        )
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn reissuing_code_revokes_old_token_and_code() {
    let app = App::new().await;
    let id = app.create_iphone("iPhone").await;
    let token = app.pair(id).await;
    let old_code = app.pair_code(id).await;
    let (st, _) = app
        .agent_call(Some(&token), Method::GET, "/api/agent/manifest", None)
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "旧トークンは即失効");
    let _new_code = app.pair_code(id).await;
    let (st, _) = app
        .agent_call(
            None,
            Method::POST,
            "/api/agent/pair",
            Some(json!({"code": old_code})),
        )
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED, "旧コードも失効");
}

// ---------------------------------------------------------------- Bearer と経路の分離

#[tokio::test]
async fn session_cookie_does_not_open_agent_routes() {
    let app = App::new().await;
    let (st, _) = app.call(Method::GET, "/api/agent/manifest", None).await; // Cookie のみ
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bearer_does_not_open_session_routes() {
    let app = App::new().await;
    let id = app.create_iphone("iPhone").await;
    let token = app.pair(id).await;
    let (st, _) = app
        .agent_call(Some(&token), Method::GET, "/api/devices", None)
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn trusted_cidr_does_not_open_agent_files() {
    let app = App::with_trusted_lan().await;
    // 前提: この App では trusted_cidrs が効いている（allowlist の経路は 401 にならない）
    let (st, _) = app
        .agent_call(None, Method::GET, "/api/stream/1", None)
        .await;
    assert_ne!(st, StatusCode::UNAUTHORIZED);
    let (st, _) = app
        .agent_call(None, Method::GET, "/api/agent/files/1", None)
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn bearer_post_needs_no_csrf_headers() {
    let app = App::new().await;
    let id = app.create_iphone("iPhone").await;
    let token = app.pair(id).await;
    // agent_call は sec-fetch-site / Origin を付けない
    let (st, _) = app
        .agent_call(
            Some(&token),
            Method::POST,
            "/api/agent/plans",
            Some(json!({"plan_token": "x"})),
        )
        .await;
    assert_ne!(st, StatusCode::FORBIDDEN);
    assert_ne!(st, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn malformed_and_wrong_tokens_are_401() {
    let app = App::new().await;
    let id = app.create_iphone("iPhone").await;
    let token = app.pair(id).await;
    let (sel, _) = token.split_once('.').unwrap();
    for bad in [
        "",
        "abc",
        &format!("{sel}.{}", "a".repeat(52)),
        &format!("{sel}."),
    ] {
        let (st, _) = app
            .agent_call(Some(bad), Method::GET, "/api/agent/manifest", None)
            .await;
        assert_eq!(st, StatusCode::UNAUTHORIZED, "{bad:?}");
    }
    let (st, _) = app
        .agent_call(Some(&token), Method::GET, "/api/agent/nope", None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unauthorized_carries_www_authenticate_bearer() {
    let app = App::new().await;
    let (st, h, _) = app
        .agent_raw(None, Method::GET, "/api/agent/manifest", &[])
        .await;
    assert_eq!(st, StatusCode::UNAUTHORIZED);
    assert_eq!(h.get("www-authenticate").unwrap(), "Bearer");
}

#[tokio::test]
async fn locked_mode_closes_agent_routes_including_pair() {
    let app = App::locked().await;
    let (st, _) = app
        .agent_call(
            None,
            Method::POST,
            "/api/agent/pair",
            Some(json!({"code": "x.y"})),
        )
        .await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
    let (st, _) = app
        .agent_call(Some("x.y"), Method::GET, "/api/agent/manifest", None)
        .await;
    assert_eq!(st, StatusCode::SERVICE_UNAVAILABLE);
}
