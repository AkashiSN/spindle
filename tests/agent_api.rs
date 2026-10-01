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
#[ignore = "Task 5 で manifest が入る"]
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
