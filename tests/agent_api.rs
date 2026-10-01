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

#[tokio::test]
async fn manifest_lists_desired_items_playlists_and_diff() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let t = app.seed_track(1, "J-Pop/A/01 a.flac", b"hello").await;
    app.seed_playlist(id, 10, "夜", &[1]).await;
    let token = app.pair(id).await;
    let (st, v) = app
        .agent_call(Some(&token), Method::GET, "/api/agent/manifest", None)
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let m: agent_proto::ManifestResponse = serde_json::from_value(v).unwrap();
    assert_eq!(m.device_name, "iPhone");
    assert_eq!(m.items.len(), 1);
    assert_eq!(m.items[0].dest_path, t.dest_path);
    assert_eq!(m.items[0].dest_path, "J-Pop/A/01 a.m4a");
    assert_eq!(m.items[0].token, t.token);
    assert_eq!(m.items[0].sha256, t.sha256);
    assert_eq!(m.items[0].size, 5);
    assert_eq!(m.playlists[0].name, "夜");
    assert_eq!(m.playlists[0].tracks, vec![1]);
    assert_eq!(m.diff.items[0].op, agent_proto::OpKind::Add);
    assert_eq!(m.diff.playlists[0].op, agent_proto::PlaylistOpKind::Add);
    assert_eq!(m.diff.playlists[0].to.as_deref(), Some("夜"));
    assert!(m.diff.held.is_empty());
    assert!(!m.pending_reevaluation);
    assert!(!m.plan_token.is_empty());
}

async fn get_file(
    app: &App,
    token: &str,
    track_id: i64,
    if_match: Option<&str>,
    range: Option<&str>,
) -> (StatusCode, HeaderMap, Vec<u8>) {
    let mut h = vec![];
    if let Some(m) = if_match {
        h.push((header::IF_MATCH, m.to_owned()));
    }
    if let Some(r) = range {
        h.push((header::RANGE, r.to_owned()));
    }
    app.agent_raw(
        Some(token),
        Method::GET,
        &format!("/api/agent/files/{track_id}"),
        &h,
    )
    .await
}

#[tokio::test]
async fn file_needs_if_match_and_serves_ranges() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let t = app.seed_track(1, "A/01 a.flac", b"0123456789").await;
    let token = app.pair(id).await;
    let etag = format!("\"{}\"", t.token);
    let (st, _, _) = get_file(&app, &token, 1, None, None).await;
    assert_eq!(st, StatusCode::PRECONDITION_REQUIRED);
    let (st, h, body) = get_file(&app, &token, 1, Some(&etag), None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(h[header::ETAG], etag.as_str());
    assert_eq!(body, b"0123456789");
    let (st, h, body) = get_file(&app, &token, 1, Some(&etag), Some("bytes=4-")).await;
    assert_eq!(st, StatusCode::PARTIAL_CONTENT);
    assert_eq!(h[header::CONTENT_RANGE], "bytes 4-9/10");
    assert_eq!(body, b"456789");
    let (st, _, _) = get_file(&app, &token, 1, Some(&etag), Some("bytes=99-")).await;
    assert_eq!(st, StatusCode::RANGE_NOT_SATISFIABLE);
}

#[tokio::test]
async fn weak_star_or_wrong_etag_is_412() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let t = app.seed_track(1, "A/01 a.flac", b"x").await;
    let token = app.pair(id).await;
    for m in [
        format!("W/\"{}\"", t.token),
        "*".to_owned(),
        format!("\"{}\"", "0".repeat(64)),
        format!("\"{}\", \"{}\"", t.token, t.token),
    ] {
        let (st, _, _) = get_file(&app, &token, 1, Some(&m), None).await;
        assert_eq!(st, StatusCode::PRECONDITION_FAILED, "{m}");
    }
}

#[tokio::test]
async fn resumed_range_after_source_changed_is_412() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let t = app.seed_track(1, "A/01 a.flac", b"0123456789").await;
    let token = app.pair(id).await;
    let etag = format!("\"{}\"", t.token);
    // 送る元が差し替わった（同じ長さで中身が違う → identity が変わる）
    app.replace_derived_bytes(1, b"abcdefghij").await;
    let (st, _, body) = get_file(&app, &token, 1, Some(&etag), Some("bytes=4-")).await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
    assert!(body.is_empty() || !body.starts_with(b"efgh"));
    // 行は消え、次の差分でハッシュが取り直される
    let gone: i64 = app
        .db
        .read(|c| {
            Ok(c.query_row(
                "SELECT count(*) FROM source_hashes WHERE track_id = 1",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(gone, 0);
}

#[tokio::test]
async fn other_device_track_is_404() {
    let app = App::with_roots().await;
    let a = app.create_iphone("A").await;
    let b = app.create_iphone("B").await;
    app.set_selection(a, "playlists").await; // A は空の選曲
    app.set_selection(b, "all").await;
    let t = app.seed_track(1, "A/01 a.flac", b"x").await;
    let token_a = app.pair(a).await;
    let (st, _, _) = get_file(&app, &token_a, 1, Some(&format!("\"{}\"", t.token)), None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}
