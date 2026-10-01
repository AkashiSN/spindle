#![cfg(feature = "fake")]
mod support;

use spindle_agent::server::{
    check_content_range, check_resume_range, Confirmed, Fetch, HttpServer, Reported, Server,
};
use spindle_agent::Error;
use support::Env;

#[test]
fn content_range_must_start_at_offset() {
    assert_eq!(check_content_range("bytes 5-9/10", 5), Some(10));
    assert_eq!(check_content_range("bytes 0-9/10", 5), None);
    assert_eq!(check_content_range("bytes 5-10/10", 5), None);
    assert_eq!(check_content_range("bytes */10", 5), None);
    assert_eq!(check_content_range("garbage", 0), None);
}

#[test]
fn resume_range_must_match_offset_and_expected_size() {
    assert!(check_resume_range("bytes 5-9/10", 5, 10));
    // 総サイズが手元の期待と違えば拒否する
    assert!(!check_resume_range("bytes 5-11/12", 5, 10));
    assert!(!check_resume_range("bytes 5-8/9", 5, 10));
    // 開始位置が違うものも拒否する
    assert!(!check_resume_range("bytes 4-9/10", 5, 10));
    assert!(!check_resume_range("garbage", 5, 10));
}

#[test]
fn http_server_refuses_plain_http_without_flag() {
    assert!(matches!(
        HttpServer::new("http://nas:8080", false, None),
        Err(Error::Stop(_))
    ));
    assert!(HttpServer::new("http://nas:8080", true, None).is_ok());
    assert!(HttpServer::new("https://music.example", false, None).is_ok());
    assert!(matches!(
        HttpServer::new("ftp://x", true, None),
        Err(Error::Stop(_))
    ));
}

#[test]
fn fake_server_diff_confirm_report_cycle() {
    let env = Env::new();
    let s = &env.server;
    s.put_track(1, "A/a.m4a", b"aaa");
    let m = s.manifest().unwrap();
    assert_eq!(m.diff.items.len(), 1);
    // 古い plan_token は Changed
    assert!(matches!(s.confirm("x").unwrap(), Confirmed::Changed { .. }));
    let Confirmed::Plan(p) = s.confirm(&m.plan_token).unwrap() else {
        panic!()
    };
    assert_eq!(p.items.len(), 1);
    // 同じトークンの確定は同じ計画（冪等）、違えば OpenPlanExists
    assert_eq!(
        s.confirm(&m.plan_token).unwrap(),
        Confirmed::Plan(p.clone())
    );
    assert_eq!(s.confirm("y").unwrap(), Confirmed::OpenPlanExists);
    let mut out = Vec::new();
    assert_eq!(
        s.fetch(1, &m.items[0].token, 3, 0, &mut out).unwrap(),
        Fetch::Complete
    );
    assert_eq!(out, b"aaa");
    let r = agent_proto::ReportRequest {
        generation: m.generation,
        plan_id: p.plan_id,
        state: agent_proto::ReportState {
            tracks: vec![agent_proto::ReportTrack {
                track_id: 1,
                dest_path: "A/a.m4a".into(),
                token: m.items[0].token.clone(),
                size: 3,
                sha256: m.items[0].sha256.clone(),
            }],
            playlists: vec![],
        },
        errors: vec![],
    };
    assert_eq!(s.report(&r).unwrap(), Reported::Ok);
    assert!(s.manifest().unwrap().diff.items.is_empty());
    assert_eq!(s.report(&r).unwrap(), Reported::Closed);
}

#[test]
fn fake_server_fetch_cut_and_changed() {
    let env = Env::new();
    let s = &env.server;
    let it = s.put_track(1, "a.m4a", b"0123456789");
    s.cut_fetch(1, 1);
    let mut out = Vec::new();
    assert!(s.fetch(1, &it.token, 10, 0, &mut out).is_err());
    assert_eq!(out, b"01234");
    assert_eq!(
        s.fetch(1, &it.token, 10, 5, &mut out).unwrap(),
        Fetch::Complete
    );
    assert_eq!(out, b"0123456789");
    s.fail_fetch_changed(1, 1);
    assert_eq!(
        s.fetch(1, &it.token, 10, 0, &mut Vec::new()).unwrap(),
        Fetch::Changed
    );
    assert_eq!(
        s.fetch(1, "other", 10, 0, &mut Vec::new()).unwrap(),
        Fetch::Changed
    );
    s.remove_track(1);
    assert_eq!(
        s.fetch(1, &it.token, 10, 0, &mut Vec::new()).unwrap(),
        Fetch::Gone
    );
}

#[test]
fn base_url_keeps_sub_path() {
    use spindle_agent::server::normalize_base;
    let u = |s: &str| normalize_base(reqwest::Url::parse(s).unwrap()).to_string();
    assert_eq!(
        u("https://nas.example/spindle"),
        "https://nas.example/spindle/"
    );
    assert_eq!(
        u("https://nas.example/spindle/"),
        "https://nas.example/spindle/"
    );
    assert_eq!(u("https://nas.example"), "https://nas.example/");
    let base = normalize_base(reqwest::Url::parse("https://nas.example/spindle").unwrap());
    assert_eq!(
        base.join("api/agent/manifest").unwrap().as_str(),
        "https://nas.example/spindle/api/agent/manifest"
    );
}
