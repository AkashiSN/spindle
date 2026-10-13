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
    assert_eq!(m.items[0].rel_path, "J-Pop/A/01 a.flac");
    assert_eq!(m.items[0].source, "aac");
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
async fn fifo_source_is_412_without_hanging() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let t = app.seed_track(1, "A/01 a.flac", b"0123456789").await;
    let token = app.pair(id).await;
    let etag = format!("\"{}\"", t.token);
    // 送る元が FIFO に差し替わった（開くと書き手を待って止まる）
    let path = app.roots.as_ref().unwrap().derived.join(&t.derived_rel);
    let tmp = path.with_extension("tmp");
    let c = std::ffi::CString::new(tmp.to_str().unwrap()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
    std::fs::rename(&tmp, &path).unwrap();
    let (st, _, _) = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        get_file(&app, &token, 1, Some(&etag), None),
    )
    .await
    .expect("FIFO で止まった");
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

#[tokio::test]
async fn report_with_stale_token_turns_into_update() {
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
        .agent_call(Some(&token), Method::POST, "/api/agent/report", Some(body))
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    // 差分が空の計画を確定し、古い写し（トークン空）として報告する
    let p2 = confirm(&app, &token).await;
    let body = report_of(
        &p2,
        json!([{"track_id": 1, "dest_path": t.dest_path, "token": "", "size": 1, "sha256": t.sha256}]),
        json!([]),
    );
    let (st, v) = app
        .agent_call(Some(&token), Method::POST, "/api/agent/report", Some(body))
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, d) = app
        .call(Method::GET, &format!("/api/devices/{id}/diff"), None)
        .await;
    assert_eq!(d["counts"]["update"], 1, "{d}");
    assert_eq!(d["counts"]["synced"], 0, "{d}");
}

#[tokio::test]
async fn confirm_after_marked_playlist_deleted_is_plan_changed() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    app.seed_track(1, "A/01 a.flac", b"x").await;
    app.seed_playlist(id, 10, "夜", &[1]).await;
    let token = app.pair(id).await;
    let (_, m) = app
        .agent_call(Some(&token), Method::GET, "/api/agent/manifest", None)
        .await;
    let (st, v) = app.call(Method::DELETE, "/api/playlists/10", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT, "{v}");
    let (st, v) = app
        .agent_call(
            Some(&token),
            Method::POST,
            "/api/agent/plans",
            Some(json!({"plan_token": m["plan_token"]})),
        )
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("plan_changed"))
    );
}

/// 終端を GC が消した後の同じ報告の再送は 404 no_plan。端末ごとに残る最後の終端への再送は 200
#[tokio::test]
async fn resend_after_plan_was_pruned_is_404_but_kept_latest_is_200() {
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
    // 閉じた計画しか無い（最後の終端）: 古くても GC は残すので再送は 200
    let old = spindle::db::now_epoch() - 400 * 86_400;
    app.db
        .write(move |c| {
            c.execute("UPDATE device_sync_plans SET closed_at = ?1", [old])?;
            Ok(())
        })
        .await
        .unwrap();
    let n = spindle::gc::prune_plans(&app.db, 7 * 86_400, spindle::db::now_epoch())
        .await
        .unwrap();
    assert_eq!(n, 0);
    let (st, _) = app
        .agent_call(
            Some(&token),
            Method::POST,
            "/api/agent/report",
            Some(body.clone()),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "残した最後の終端への再送");
    // 次の計画を閉じると、最初の計画は「最後」ではなくなり、古ければ消える
    let p2 = confirm(&app, &token).await;
    assert_ne!(p2.plan_id, p.plan_id);
    let body2 = report_of(&p2, json!([]), json!([]));
    let (st, v) = app
        .agent_call(
            Some(&token),
            Method::POST,
            "/api/agent/report",
            Some(body2.clone()),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let n = spindle::gc::prune_plans(&app.db, 7 * 86_400, spindle::db::now_epoch() + 10 * 86_400)
        .await
        .unwrap();
    assert_eq!(n, 1, "最初の計画だけが消える");
    let (st, v) = app
        .agent_call(Some(&token), Method::POST, "/api/agent/report", Some(body))
        .await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::NOT_FOUND, Some("no_plan"))
    );
    let (st, _) = app
        .agent_call(Some(&token), Method::POST, "/api/agent/report", Some(body2))
        .await;
    assert_eq!(st, StatusCode::OK, "最後の終端への再送は 200");
}

// ---------------------------------------------------------------- pair --adopt と adopt（P5-8）

const OLD_UUID: &str = "0123456789abcdef0123456789abcdef";

async fn pair_with(app: &App, code: &str, adopt_uuid: Option<&str>) -> (StatusCode, Value) {
    let body = match adopt_uuid {
        Some(u) => json!({"code": code, "adopt_uuid": u}),
        None => json!({"code": code}),
    };
    app.agent_call(None, Method::POST, "/api/agent/pair", Some(body))
        .await
}

async fn sql(app: &App, q: &'static str, id: i64) {
    app.db
        .write(move |c| {
            c.execute(q, [id])?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn device_row(app: &App, id: i64) -> (String, i64, Option<i64>, i64) {
    app.db
        .read(move |c| {
            Ok(c.query_row(
                "SELECT uuid, generation, last_synced_at, pair_code_attempts FROM devices WHERE id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn pair_adopt_swaps_the_uuid_of_an_unused_row() {
    let app = App::new().await;
    let id = app.create_iphone("iPhone").await;
    let code = app.pair_code(id).await;
    let (st, v) = pair_with(&app, &code, Some(OLD_UUID)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!(v["device_uuid"], OLD_UUID);
    assert_eq!(device_row(&app, id).await.0, OLD_UUID);
    let token = v["token"].as_str().unwrap();
    let (st, _) = app
        .agent_call(
            Some(token),
            Method::POST,
            "/api/agent/plans",
            Some(json!({"plan_token": "x"})),
        )
        .await;
    assert_ne!(st, StatusCode::UNAUTHORIZED, "トークンが使える");
}

#[tokio::test]
async fn pair_adopt_is_idempotent_on_retry() {
    let app = App::new().await;
    let id = app.create_iphone("iPhone").await;
    let code = app.pair_code(id).await;
    let (st, v) = pair_with(&app, &code, Some(OLD_UUID)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let code = app.pair_code(id).await;
    let (st, v) = pair_with(&app, &code, Some(OLD_UUID)).await;
    assert_eq!(st, StatusCode::OK, "差し替え済みのやり直し: {v}");
    assert_eq!(v["device_uuid"], OLD_UUID);
}

/// 引き継ぎの後（adopt でキャッシュを書いた後）、finish を保存する前に落ちてもう一度 pair --adopt しても通る
#[tokio::test]
async fn pair_adopt_retry_after_adopt_wrote_the_cache_succeeds() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let t1 = app.seed_track(1, "A/01 a.flac", b"aaa").await;
    insert_playlist(&app, 10, "夜").await;
    let code = app.pair_code(id).await;
    let (st, v) = pair_with(&app, &code, Some(OLD_UUID)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let token = v["token"].as_str().unwrap().to_owned();
    let body = json!({
        "tracks": [adopt_track(70, &t1.dest_path, "A/01 a.flac", 3, &t1.sha256, false)],
        "playlists": [{"playlist_id": 50, "name": "夜"}],
    });
    let (st, v) = adopt(&app, &token, body).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let code = app.pair_code(id).await;
    let (st, v) = pair_with(&app, &code, Some(OLD_UUID)).await;
    assert_eq!(
        st,
        StatusCode::OK,
        "キャッシュがあっても報告前ならやり直せる: {v}"
    );
    assert_eq!(v["device_uuid"], OLD_UUID);
}

/// uuid が既に adopt_uuid でも、報告・計画のある行（DB を失っていない Mac）は引き継ぎを断り、
/// コードを消費しない（素の pair で入れ直せる）
#[tokio::test]
async fn pair_adopt_refuses_a_used_row_with_the_same_uuid_without_consuming_the_code() {
    let setups: [&'static str; 2] = [
        "UPDATE devices SET last_synced_at = 1 WHERE id = ?1",
        "INSERT INTO device_sync_plans (device_id, plan_token, plan, state, created_at)
         VALUES (?1, 'x', '{}', 'completed', 0)",
    ];
    for q in setups {
        let app = App::new().await;
        let id = app.create_iphone("iPhone").await;
        let code = app.pair_code(id).await;
        let (st, v) = pair_with(&app, &code, Some(OLD_UUID)).await;
        assert_eq!(st, StatusCode::OK, "{v}");
        sql(&app, q, id).await;
        let code = app.pair_code(id).await;
        let (st, v) = pair_with(&app, &code, Some(OLD_UUID)).await;
        assert_eq!(
            (st, v["error"].as_str()),
            (StatusCode::CONFLICT, Some("not_adoptable")),
            "{q}"
        );
        let (uuid, _, _, attempts) = device_row(&app, id).await;
        assert_eq!(uuid, OLD_UUID, "{q}");
        assert_eq!(attempts, 0, "試行に数えない: {q}");
        let (st, v) = pair_with(&app, &code, None).await;
        assert_eq!(st, StatusCode::OK, "コードは消費されていない: {q}: {v}");
        assert_eq!(v["device_uuid"], OLD_UUID);
    }
}

#[tokio::test]
async fn pair_adopt_refuses_a_used_row_without_consuming_the_code() {
    let setups: [&'static str; 4] = [
        "UPDATE devices SET last_synced_at = 1 WHERE id = ?1",
        "INSERT INTO device_sync_plans (device_id, plan_token, plan, state, created_at)
         VALUES (?1, 'x', '{}', 'completed', 0)",
        "INSERT INTO device_items (device_id, track_id, dest_path, dest_path_key, token, size, sha256, synced_at)
         VALUES (?1, 1, 'a.m4a', 'a.m4a', 't', 1, 's', 0)",
        "INSERT INTO device_playlist_state (device_id, playlist_id, dest_path, token, synced_at)
         VALUES (?1, 1, '夜', 't', 0)",
    ];
    for q in setups {
        let app = App::new().await;
        let id = app.create_iphone("iPhone").await;
        let code = app.pair_code(id).await;
        sql(&app, q, id).await;
        let before = device_row(&app, id).await.0;
        let (st, v) = pair_with(&app, &code, Some(OLD_UUID)).await;
        assert_eq!(
            (st, v["error"].as_str()),
            (StatusCode::CONFLICT, Some("not_adoptable")),
            "{q}"
        );
        let (uuid, _, _, attempts) = device_row(&app, id).await;
        assert_eq!(uuid, before, "{q}");
        assert_eq!(attempts, 0, "試行に数えない: {q}");
        let (st, v) = pair_with(&app, &code, None).await;
        assert_eq!(st, StatusCode::OK, "コードは消費されていない: {q}: {v}");
        assert_eq!(v["device_uuid"], before.as_str());
    }
}

#[tokio::test]
async fn pair_adopt_refuses_a_uuid_of_another_row() {
    let app = App::new().await;
    let a = app.create_iphone("iPhone").await;
    let b = app.create_iphone("iPad").await;
    let other = device_row(&app, b).await.0;
    let code = app.pair_code(a).await;
    let (st, v) = pair_with(&app, &code, Some(&other)).await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_adoptable"))
    );
    let (st, v) = pair_with(&app, &code, None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_ne!(v["device_uuid"], other.as_str());
}

#[tokio::test]
async fn pair_adopt_rejects_a_malformed_uuid() {
    let app = App::new().await;
    let id = app.create_iphone("iPhone").await;
    let code = app.pair_code(id).await;
    for bad in ["OLD", "0123456789ABCDEF0123456789ABCDEF", ""] {
        let (st, v) = pair_with(&app, &code, Some(bad)).await;
        assert_eq!(
            (st, v["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("bad_request")),
            "{bad}"
        );
    }
    assert_eq!(device_row(&app, id).await.3, 0, "試行に数えない");
    let (st, v) = pair_with(&app, &code, None).await;
    assert_eq!(st, StatusCode::OK, "{v}");
}

/// 印の付いていないプレイリストを入れる
async fn insert_playlist(app: &App, playlist_id: i64, name: &str) {
    let name = name.to_owned();
    app.db
        .write(move |c| {
            c.execute(
                "INSERT INTO playlists (id, name, name_key, kind, created_at, updated_at)
                 VALUES (?1, ?2, ?3, 'manual', 0, 0)",
                rusqlite::params![
                    playlist_id,
                    name,
                    spindle::domain::relpath::canonical_key(&name)
                ],
            )?;
            Ok(())
        })
        .await
        .unwrap();
}

type Rows = (
    Vec<(i64, String, String, i64, String)>,
    Vec<(i64, String, String)>,
    Vec<i64>,
    i64,
);

/// device_items・device_playlist_state・印・device_errors の件数
async fn cache_of(app: &App, id: i64) -> Rows {
    app.db
        .read(move |c| {
            let mut st = c.prepare(
                "SELECT track_id, dest_path, token, size, sha256 FROM device_items
                 WHERE device_id = ?1 ORDER BY track_id",
            )?;
            let items = st
                .query_map([id], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut st = c.prepare(
                "SELECT playlist_id, dest_path, token FROM device_playlist_state
                 WHERE device_id = ?1 ORDER BY playlist_id",
            )?;
            let pls = st
                .query_map([id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut st = c.prepare(
                "SELECT playlist_id FROM device_playlists WHERE device_id = ?1 ORDER BY playlist_id",
            )?;
            let marks = st
                .query_map([id], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let errors: i64 = c.query_row(
                "SELECT COUNT(*) FROM device_errors WHERE device_id = ?1",
                [id],
                |r| r.get(0),
            )?;
            Ok((items, pls, marks, errors))
        })
        .await
        .unwrap()
}

async fn adopt(app: &App, token: &str, body: Value) -> (StatusCode, Value) {
    app.agent_call(Some(token), Method::POST, "/api/agent/adopt", Some(body))
        .await
}

fn adopt_track(id: i64, path: &str, rel: &str, size: u64, sha: &str, stale: bool) -> Value {
    json!({"track_id": id, "path": path, "rel_path": rel, "source": "aac",
           "size": size, "sha256": sha, "stale": stale})
}

#[tokio::test]
async fn adopt_remaps_and_writes_the_cache() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let t1 = app.seed_track(1, "A/01 a.flac", b"aaa").await;
    app.seed_track(2, "B/01 b.flac", b"bb").await;
    insert_playlist(&app, 10, "夜").await;
    let code = app.pair_code(id).await;
    let (_, v) = pair_with(&app, &code, Some(OLD_UUID)).await;
    let token = v["token"].as_str().unwrap().to_owned();
    sql(
        &app,
        "INSERT INTO device_errors (device_id, kind, ref_id, reason, reported_at)
         VALUES (?1, 'track', 1, 'x', 0)",
        id,
    )
    .await;
    let gen_before = device_row(&app, id).await.1;
    let body = json!({
        "tracks": [
            adopt_track(70, &t1.dest_path, "A/01 a.flac", 3, &t1.sha256, false),
            adopt_track(80, "B/01 b.m4a", "b/01 B.flac", 9, &"b".repeat(64), true),
            adopt_track(90, "C/x.m4a", "C/gone.flac", 1, "", true),
        ],
        "playlists": [{"playlist_id": 50, "name": "夜"}, {"playlist_id": 51, "name": "Gone"}],
    });
    let (st, v) = adopt(&app, &token, body).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let r: agent_proto::AdoptResponse = serde_json::from_value(v).unwrap();
    let m = |from, to| agent_proto::IdMap { from, to };
    assert_eq!(r.tracks, vec![m(70, Some(1)), m(80, Some(2)), m(90, None)]);
    assert_eq!(r.playlists, vec![m(50, Some(10)), m(51, None)]);

    let (items, pls, marks, errors) = cache_of(&app, id).await;
    assert_eq!(
        items,
        vec![
            (
                1,
                t1.dest_path.clone(),
                "adopted".into(),
                3,
                t1.sha256.clone()
            ),
            (2, "B/01 b.m4a".into(), "".into(), 9, "b".repeat(64)),
        ]
    );
    assert_eq!(pls, vec![(10, "夜".into(), "".into())]);
    assert_eq!(marks, vec![10]);
    assert_eq!(errors, 0);
    let (_, gen_after, synced, _) = device_row(&app, id).await;
    assert_eq!(gen_after, gen_before, "generation は進めない");
    assert_eq!(synced, None);
}

#[tokio::test]
async fn adopt_is_idempotent() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let t1 = app.seed_track(1, "A/01 a.flac", b"aaa").await;
    insert_playlist(&app, 10, "夜").await;
    let token = app.pair(id).await;
    let body = json!({
        "tracks": [adopt_track(70, &t1.dest_path, "A/01 a.flac", 3, &t1.sha256, false)],
        "playlists": [{"playlist_id": 50, "name": "夜"}],
    });
    let (st, first) = adopt(&app, &token, body.clone()).await;
    assert_eq!(st, StatusCode::OK, "{first}");
    let cache = cache_of(&app, id).await;
    let (st, second) = adopt(&app, &token, body).await;
    assert_eq!(st, StatusCode::OK, "{second}");
    assert_eq!(first, second);
    assert_eq!(cache_of(&app, id).await, cache);
}

#[tokio::test]
async fn adopt_refuses_open_plan_reported_device_and_scan() {
    let body = json!({"tracks": [], "playlists": []});
    // open な計画
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    app.seed_track(1, "A/01 a.flac", b"aaa").await;
    let token = app.pair(id).await;
    confirm(&app, &token).await;
    let (st, v) = adopt(&app, &token, body.clone()).await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("open_plan"))
    );
    // 報告を受けたことがある
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let token = app.pair(id).await;
    sql(
        &app,
        "UPDATE devices SET last_synced_at = 1 WHERE id = ?1",
        id,
    )
    .await;
    let (st, v) = adopt(&app, &token, body.clone()).await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("not_adoptable"))
    );
    // スキャン中
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let token = app.pair(id).await;
    sql(
        &app,
        "INSERT INTO jobs (type, state, payload, created_at) SELECT 'scan', 'queued', '{}', ?1",
        0,
    )
    .await;
    let (st, v) = adopt(&app, &token, body).await;
    assert_eq!(
        (st, v["error"].as_str()),
        (StatusCode::CONFLICT, Some("library_busy"))
    );
}

#[tokio::test]
async fn adopt_rejects_bad_input() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    app.seed_track(1, "A/01 a.flac", b"aaa").await;
    let token = app.pair(id).await;
    let hex = "a".repeat(64);
    let ok = |id, path: &str| adopt_track(id, path, "A/01 a.flac", 3, &hex, false);
    let mut unknown_source = ok(1, "A/a.m4a");
    unknown_source["source"] = json!("mp3");
    let many: Vec<Value> = (0..=spindle::device::report::MAX_PLAYLISTS as i64)
        .map(|i| json!({"playlist_id": i, "name": "n"}))
        .collect();
    let cases = [
        (
            "重複 id",
            json!({"tracks": [ok(1, "A/a.m4a"), ok(1, "A/b.m4a")], "playlists": []}),
        ),
        (
            "重複パス",
            json!({"tracks": [ok(1, "A/a.m4a"), ok(2, "a/A.m4a")], "playlists": []}),
        ),
        (
            "..",
            json!({"tracks": [ok(1, "A/../a.m4a")], "playlists": []}),
        ),
        (
            "予約名",
            json!({"tracks": [ok(1, ".spindle/a.m4a")], "playlists": []}),
        ),
        (
            "rel_path",
            json!({"tracks": [adopt_track(1, "A/a.m4a", "/abs.flac", 3, &hex, false)], "playlists": []}),
        ),
        (
            "source",
            json!({"tracks": [unknown_source], "playlists": []}),
        ),
        (
            "プレイリストの重複 id",
            json!({"tracks": [], "playlists": [{"playlist_id": 1, "name": "a"}, {"playlist_id": 1, "name": "b"}]}),
        ),
        (
            "空の名前",
            json!({"tracks": [], "playlists": [{"playlist_id": 1, "name": ""}]}),
        ),
        ("件数", json!({"tracks": [], "playlists": many})),
        (
            "sha256 が短い",
            json!({"tracks": [adopt_track(1, "A/a.m4a", "A/01 a.flac", 3, "s", false)], "playlists": []}),
        ),
        (
            "sha256 が大文字",
            json!({"tracks": [adopt_track(1, "A/a.m4a", "A/01 a.flac", 3, &"A".repeat(64), false)], "playlists": []}),
        ),
        (
            "sha256 が 16 進でない",
            json!({"tracks": [adopt_track(1, "A/a.m4a", "A/01 a.flac", 3, &"g".repeat(64), false)], "playlists": []}),
        ),
        (
            "stale でないのに sha256 が空",
            json!({"tracks": [adopt_track(1, "A/a.m4a", "A/01 a.flac", 3, "", false)], "playlists": []}),
        ),
        (
            "size が i64 を超える",
            json!({"tracks": [adopt_track(1, "A/a.m4a", "A/01 a.flac", i64::MAX as u64 + 1, &hex, false)], "playlists": []}),
        ),
    ];
    for (what, body) in cases {
        let (st, v) = adopt(&app, &token, body).await;
        assert_eq!(
            (st, v["error"].as_str()),
            (StatusCode::BAD_REQUEST, Some("bad_request")),
            "{what}: {v}"
        );
    }
    let (items, pls, _, _) = cache_of(&app, id).await;
    assert!(items.is_empty() && pls.is_empty());
}

#[tokio::test]
async fn adopted_diff_is_adoption_check_and_report_passes() {
    let app = App::with_roots().await;
    let id = app.create_iphone("iPhone").await;
    let t = app.seed_track(1, "A/01 a.flac", b"aaa").await;
    let code = app.pair_code(id).await;
    let (_, v) = pair_with(&app, &code, Some(OLD_UUID)).await;
    let token = v["token"].as_str().unwrap().to_owned();
    let body = json!({
        "tracks": [adopt_track(70, &t.dest_path, "A/01 a.flac", 3, &t.sha256, false)],
        "playlists": [],
    });
    let (st, v) = adopt(&app, &token, body).await;
    assert_eq!(st, StatusCode::OK, "{v}");

    let (st, m) = app
        .agent_call(Some(&token), Method::GET, "/api/agent/manifest", None)
        .await;
    assert_eq!(st, StatusCode::OK, "{m}");
    let m: agent_proto::ManifestResponse = serde_json::from_value(m).unwrap();
    assert_eq!(m.diff.items.len(), 1);
    assert_eq!(m.diff.items[0].op, agent_proto::OpKind::Update);
    let (st, d) = app
        .call(Method::GET, &format!("/api/devices/{id}/diff"), None)
        .await;
    assert_eq!(st, StatusCode::OK, "{d}");
    assert_eq!(d["items"][0]["op"], "adopted", "{d}");

    let p = confirm(&app, &token).await;
    assert_eq!(p.items[0].token.as_deref(), Some(t.token.as_str()));
    let report = report_of(
        &p,
        json!([{"track_id": 1, "dest_path": t.dest_path, "token": t.token, "size": 3, "sha256": t.sha256}]),
        json!([]),
    );
    let (st, v) = app
        .agent_call(
            Some(&token),
            Method::POST,
            "/api/agent/report",
            Some(report),
        )
        .await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let (_, d) = app
        .call(Method::GET, &format!("/api/devices/{id}/diff"), None)
        .await;
    assert_eq!(d["counts"]["synced"], 1);
}
