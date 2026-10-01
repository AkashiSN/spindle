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
    let (st, v) = app
        .agent_call(
            Some(&token),
            Method::POST,
            "/api/agent/plans",
            Some(json!({"plan_token": "x"})),
        )
        .await;
    // CSRF・認証で落ちず、計画の確定まで届く（計算し直した plan_token と違うので 409 plan_changed）
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("plan_changed"))
    );
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
async fn symlinked_source_is_412_and_hash_row_is_forgotten() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let t = app.seed_track(1, "A/01 a.flac", b"0123456789").await;
    let token = app.pair(id).await;
    let etag = format!("\"{}\"", t.token);
    // 送る元が symlink に差し替わった（中身は同じコピーを指す）
    let path = app.roots.as_ref().unwrap().derived.join(&t.derived_rel);
    let copy = path.with_extension("copy");
    std::fs::copy(&path, &copy).unwrap();
    let tmp = path.with_extension("tmp");
    std::os::unix::fs::symlink(&copy, &tmp).unwrap();
    std::fs::rename(&tmp, &path).unwrap();
    let (st, _, _) = get_file(&app, &token, 1, Some(&etag), None).await;
    assert_eq!(st, StatusCode::PRECONDITION_FAILED);
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

async fn confirm(app: &App, token: &str) -> agent_proto::Plan {
    let (_, m) = app
        .agent_call(Some(token), Method::GET, "/api/agent/manifest", None)
        .await;
    let (st, v) = app
        .agent_call(
            Some(token),
            Method::POST,
            "/api/agent/plans",
            Some(json!({"plan_token": m["plan_token"]})),
        )
        .await;
    assert_eq!(st, StatusCode::CREATED, "{v}");
    serde_json::from_value(v).unwrap()
}

fn report_of(plan: &agent_proto::Plan, tracks: Value, errors: Value) -> Value {
    json!({"generation": plan.generation, "plan_id": plan.plan_id,
           "state": {"tracks": tracks, "playlists": []}, "errors": errors})
}

#[tokio::test]
async fn confirm_is_idempotent_and_conflicts() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    app.seed_track(1, "A/01 a.flac", b"x").await;
    let token = app.pair(id).await;
    let p = confirm(&app, &token).await;
    let (st, v) = app
        .agent_call(
            Some(&token),
            Method::POST,
            "/api/agent/plans",
            Some(json!({"plan_token": p.plan_token})),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "同じ plan_token は同じ計画: {v}");
    assert_eq!(v["plan_id"], p.plan_id);
    let (st, v) = app
        .agent_call(
            Some(&token),
            Method::POST,
            "/api/agent/plans",
            Some(json!({"plan_token": "other"})),
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("open_plan_exists"))
    );
    let (st, v) = app
        .agent_call(Some(&token), Method::GET, "/api/agent/plans/open", None)
        .await;
    assert_eq!(
        (st, v["plan_id"].as_i64()),
        (StatusCode::OK, Some(p.plan_id))
    );
    // open な計画の間は UI の PATCH（名前以外）・PUT playlists・pair-code が 409、名前だけの PATCH は通る
    let (st, _) = app
        .call(
            Method::PATCH,
            &format!("/api/devices/{id}"),
            Some(json!({"selection": "playlists"})),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = app
        .call(
            Method::PUT,
            &format!("/api/devices/{id}/playlists"),
            Some(json!({"playlist_ids": []})),
        )
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = app
        .call(Method::POST, &format!("/api/devices/{id}/pair-code"), None)
        .await;
    assert_eq!(st, StatusCode::CONFLICT);
    let (st, _) = app
        .call(
            Method::PATCH,
            &format!("/api/devices/{id}"),
            Some(json!({"name": "iPhone 2"})),
        )
        .await;
    assert_eq!(st, StatusCode::OK);
}

#[tokio::test]
async fn stale_plan_token_is_409_with_current_token() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    app.seed_track(1, "A/01 a.flac", b"x").await;
    let token = app.pair(id).await;
    let (st, v) = app
        .agent_call(
            Some(&token),
            Method::POST,
            "/api/agent/plans",
            Some(json!({"plan_token": "stale"})),
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("plan_changed"))
    );
    assert!(v["plan_token"].as_str().is_some());
}

#[tokio::test]
async fn report_applies_state_closes_plan_and_resend_is_200() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let t = app.seed_track(1, "A/01 a.flac", b"x").await;
    let token = app.pair(id).await;
    let p = confirm(&app, &token).await;
    let body = report_of(
        &p,
        json!([{"track_id": 1, "dest_path": t.dest_path, "token": t.token, "size": 1, "sha256": t.sha256}]),
        json!([]),
    );
    let (st, v) = app
        .agent_call(
            Some(&token),
            Method::POST,
            "/api/agent/report",
            Some(body.clone()),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, d) = app
        .call(Method::GET, &format!("/api/devices/{id}/diff"), None)
        .await;
    assert_eq!(d["counts"]["synced"], 1);
    let (st, _) = app
        .agent_call(
            Some(&token),
            Method::POST,
            "/api/agent/report",
            Some(body.clone()),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "同じ報告の再送");
    let mut other = body;
    other["errors"] = json!([{"kind": "track", "ref_id": 1, "reason": "x"}]);
    let (st, v) = app
        .agent_call(Some(&token), Method::POST, "/api/agent/report", Some(other))
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("plan_closed"))
    );
}

#[tokio::test]
async fn invalid_report_is_400_and_changes_nothing() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let t = app.seed_track(1, "A/01 a.flac", b"x").await;
    let token = app.pair(id).await;
    let p = confirm(&app, &token).await;
    for bad in [
        json!([{"track_id": 1, "dest_path": "../x.m4a", "token": t.token, "size": 1, "sha256": t.sha256}]),
        json!([{"track_id": 99, "dest_path": "Z/z.m4a", "token": "t", "size": 1, "sha256": "s"}]),
    ] {
        let (st, v) = app
            .agent_call(
                Some(&token),
                Method::POST,
                "/api/agent/report",
                Some(report_of(&p, bad, json!([]))),
            )
            .await;
        assert_eq!(
            (st, v["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("invalid_report"))
        );
    }
    let (st, v) = app
        .agent_call(Some(&token), Method::GET, "/api/agent/plans/open", None)
        .await;
    assert_eq!(
        (st, v["plan_id"].as_i64()),
        (StatusCode::OK, Some(p.plan_id)),
        "計画は open のまま"
    );
    let n: i64 = app
        .db
        .read(|c| Ok(c.query_row("SELECT count(*) FROM device_items", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(n, 0);
}

#[tokio::test]
async fn generation_mismatch_is_409() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    app.seed_track(1, "A/01 a.flac", b"x").await;
    let token = app.pair(id).await;
    let p = confirm(&app, &token).await;
    let mut body = report_of(&p, json!([]), json!([]));
    body["generation"] = json!(p.generation + 1);
    let (st, v) = app
        .agent_call(Some(&token), Method::POST, "/api/agent/report", Some(body))
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("generation_mismatch"))
    );
}

#[tokio::test]
async fn abandon_requires_empty_pending_and_valid_state() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    app.seed_track(1, "A/01 a.flac", b"x").await;
    let token = app.pair(id).await;
    let p = confirm(&app, &token).await;
    let uri = format!("/api/agent/plans/{}/abandon", p.plan_id);
    let mut body = report_of(&p, json!([]), json!([]));
    body["pending_ops"] = json!(1);
    body["pending_batches"] = json!(0);
    let (st, v) = app
        .agent_call(Some(&token), Method::POST, &uri, Some(body.clone()))
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("pending_work"))
    );
    body["pending_ops"] = json!(0);
    let (st, v) = app
        .agent_call(Some(&token), Method::POST, &uri, Some(body))
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (st, _) = app
        .agent_call(Some(&token), Method::GET, "/api/agent/plans/open", None)
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn cannot_report_on_other_devices_plan() {
    let app = App::with_roots().await;
    let a = app.create_iphone("A").await;
    let b = app.create_iphone("B").await;
    app.seed_track(1, "A/01 a.flac", b"x").await;
    let (ta, tb) = (app.pair(a).await, app.pair(b).await);
    let p = confirm(&app, &ta).await;
    let (st, _) = app
        .agent_call(
            Some(&tb),
            Method::POST,
            "/api/agent/report",
            Some(report_of(&p, json!([]), json!([]))),
        )
        .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn unreadable_plan_is_409_and_changes_nothing() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let t = app.seed_track(1, "A/01 a.flac", b"x").await;
    let token = app.pair(id).await;
    let p = confirm(&app, &token).await;
    let pid = p.plan_id;
    app.db
        .write(move |c| {
            c.execute(
                "UPDATE device_sync_plans SET plan = '[1]' WHERE id = ?1",
                [pid],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let (st, v) = app
        .agent_call(Some(&token), Method::GET, "/api/agent/plans/open", None)
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("plan_unreadable"))
    );
    let body = report_of(
        &p,
        json!([{"track_id": 1, "dest_path": t.dest_path, "token": t.token, "size": 1, "sha256": t.sha256}]),
        json!([]),
    );
    let (st, v) = app
        .agent_call(Some(&token), Method::POST, "/api/agent/report", Some(body))
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("plan_unreadable"))
    );
    let (state, n): (String, i64) = app
        .db
        .read(move |c| {
            Ok((
                c.query_row(
                    "SELECT state FROM device_sync_plans WHERE id = ?1",
                    [pid],
                    |r| r.get(0),
                )?,
                c.query_row("SELECT count(*) FROM device_items", [], |r| r.get(0))?,
            ))
        })
        .await
        .unwrap();
    assert_eq!((state.as_str(), n), ("open", 0));
}
