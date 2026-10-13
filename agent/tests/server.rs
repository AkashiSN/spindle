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

/// 1 接続だけ受けて、要求（先頭行 + 本文）を返す使い捨ての HTTP サーバ
fn mock_once(status: &str, body: &'static str) -> (String, std::thread::JoinHandle<String>) {
    use std::io::{Read, Write};
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let status = status.to_owned();
    let h = std::thread::spawn(move || {
        let (mut c, _) = l.accept().unwrap();
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = c.read(&mut tmp).unwrap();
            buf.extend_from_slice(&tmp[..n]);
            let text = String::from_utf8_lossy(&buf).into_owned();
            if let Some((head, rest)) = text.split_once("\r\n\r\n") {
                let len = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if rest.len() >= len {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let resp = format!(
            "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        c.write_all(resp.as_bytes()).unwrap();
        String::from_utf8_lossy(&buf).into_owned()
    });
    (url, h)
}

fn adopt_req() -> agent_proto::AdoptRequest {
    agent_proto::AdoptRequest {
        tracks: vec![agent_proto::AdoptTrack {
            track_id: 7,
            path: "A/a.m4a".into(),
            rel_path: "Artist/a.flac".into(),
            source: "master".into(),
            size: 3,
            sha256: "ab".into(),
            stale: false,
        }],
        playlists: vec![agent_proto::AdoptPlaylist {
            playlist_id: 2,
            name: "P".into(),
        }],
    }
}

#[test]
fn http_adopt_posts_the_body_and_reads_the_map() {
    let (url, h) = mock_once(
        "200 OK",
        r#"{"tracks":[{"from":7,"to":70},{"from":8,"to":null}],"playlists":[{"from":2,"to":20}]}"#,
    );
    let s = HttpServer::new(&url, true, Some("tok".into())).unwrap();
    let r = s.adopt(&adopt_req()).unwrap();
    assert_eq!(r.tracks[0].to, Some(70));
    assert_eq!(r.tracks[1].to, None);
    assert_eq!(r.playlists[0].to, Some(20));
    let req = h.join().unwrap();
    assert!(req.starts_with("POST /api/agent/adopt "), "{req}");
    assert!(req.contains("\"rel_path\":\"Artist/a.flac\""), "{req}");
    assert!(req
        .to_ascii_lowercase()
        .contains("authorization: bearer tok"));
}

#[test]
fn http_adopt_409_codes_are_japanese_stops() {
    for (code, want) in [
        (
            "library_busy",
            "サーバのライブラリのスキャン中。終わってから sync し直してください",
        ),
        ("open_plan", "途中の計画"),
        ("not_adoptable", "引き継げる状態ではありません"),
    ] {
        let body: &'static str = Box::leak(format!(r#"{{"error":"{code}"}}"#).into_boxed_str());
        let (url, h) = mock_once("409 Conflict", body);
        let s = HttpServer::new(&url, true, Some("tok".into())).unwrap();
        match s.adopt(&adopt_req()) {
            Err(Error::Stop(m)) => assert!(m.contains(want), "{code}: {m}"),
            other => panic!("{code}: {other:?}"),
        }
        h.join().unwrap();
    }
}

#[test]
fn http_pair_sends_adopt_uuid_and_maps_not_adoptable() {
    let (url, h) = mock_once(
        "200 OK",
        r#"{"device_uuid":"u","device_name":"n","token":"t"}"#,
    );
    let s = HttpServer::new(&url, true, None).unwrap();
    s.pair("123456", Some("0123456789abcdef0123456789abcdef"))
        .unwrap();
    let req = h.join().unwrap();
    assert!(
        req.contains("\"adopt_uuid\":\"0123456789abcdef0123456789abcdef\""),
        "{req}"
    );

    let (url, h) = mock_once("409 Conflict", r#"{"error":"not_adoptable"}"#);
    let s = HttpServer::new(&url, true, None).unwrap();
    match s.pair("123456", Some("0123456789abcdef0123456789abcdef")) {
        Err(Error::Stop(m)) => {
            assert!(m.contains("新しい iPhone 端末を作ってから pair し直してください"))
        }
        other => panic!("{other:?}"),
    }
    h.join().unwrap();
}

#[test]
fn fake_server_adopt_maps_by_rel_path_and_name() {
    let env = Env::new();
    let s = &env.server;
    s.put_track(70, "X/a.m4a", b"a");
    s.set_origin(70, "Artist/a.flac", "master");
    s.put_playlist(20, "P", &[70]);
    let r = s.adopt(&adopt_req()).unwrap();
    assert_eq!(r.tracks[0].to, Some(70));
    assert_eq!(r.playlists[0].to, Some(20));
    s.fail_adopt(1);
    assert!(s.adopt(&adopt_req()).is_err());
    assert_eq!(s.adopt_calls().len(), 2);
    // pair は adopt_uuid を記録し、その uuid を返す。断られれば Stop
    let u = "0123456789abcdef0123456789abcdef";
    assert_eq!(s.pair("1", Some(u)).unwrap().device_uuid, u);
    assert_eq!(s.last_pair_adopt_uuid().as_deref(), Some(u));
    s.set_adopt_refused(true);
    assert!(matches!(s.pair("1", Some(u)), Err(Error::Stop(_))));
    assert!(s.pair("1", None).is_ok());
    assert_eq!(s.last_pair_adopt_uuid(), None);
}
