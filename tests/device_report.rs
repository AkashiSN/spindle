use agent_proto::*;
use spindle::device::plan::{PlanItem, StoredPlan};
use spindle::device::report::{digest, validate, Basis};
use spindle::domain::device::{DeviceItem, OpKind as K};

fn item(id: i64, path: &str, token: &str) -> DeviceItem {
    DeviceItem {
        track_id: id,
        dest_path: path.into(),
        token: token.into(),
        size: 10,
        sha256: format!("s{id}"),
    }
}
fn rt(i: &DeviceItem) -> ReportTrack {
    ReportTrack {
        track_id: i.track_id,
        dest_path: i.dest_path.clone(),
        token: i.token.clone(),
        size: i.size,
        sha256: i.sha256.clone(),
    }
}
fn req(tracks: Vec<ReportTrack>) -> ReportRequest {
    ReportRequest {
        generation: 1,
        plan_id: 7,
        state: ReportState {
            tracks,
            playlists: vec![],
        },
        errors: vec![],
    }
}
fn plan(items: Vec<PlanItem>) -> StoredPlan {
    StoredPlan {
        v: 1,
        generation: 1,
        plan_token: "p".into(),
        items,
        playlists: vec![],
    }
}
fn add(id: i64, to: &str, token: &str) -> PlanItem {
    PlanItem {
        op_id: format!("o{id}"),
        op: K::Add,
        track_id: id,
        from: None,
        to: Some(to.into()),
        token: Some(token.into()),
        size: 10,
        sha256: Some(format!("s{id}")),
    }
}

#[test]
fn accepts_current_rows_and_plan_results() {
    let cur = vec![item(1, "a/1.m4a", "t1")];
    let p = plan(vec![add(2, "a/2.m4a", "t2")]);
    let b = Basis {
        desired: &[],
        desired_playlists: &[],
        current: &cur,
        current_playlists: &[],
        plan: &p,
    };
    let ok = validate(&req(vec![rt(&cur[0]), rt(&item(2, "a/2.m4a", "t2"))]), &b).unwrap();
    assert_eq!(ok.items.len(), 2);
    // 何も無い（全部消えた）報告も通る
    assert!(validate(&req(vec![]), &b).is_ok());
}

#[test]
fn rejects_unknown_resurrected_duplicate_and_bad_paths() {
    let cur = vec![item(1, "a/1.m4a", "t1")];
    let p = plan(vec![]);
    let b = Basis {
        desired: &[],
        desired_playlists: &[],
        current: &cur,
        current_playlists: &[],
        plan: &p,
    };
    // どれにも一致しない（トークン違い）
    assert!(validate(&req(vec![rt(&item(1, "a/1.m4a", "zz"))]), &b).is_err());
    // current から消えた曲の復活
    assert!(validate(&req(vec![rt(&item(3, "a/3.m4a", "t3"))]), &b).is_err());
    // ..・先頭 / ・予約名
    for bad in ["../x.m4a", "/x.m4a", ".spindle/x", "a/./b.m4a"] {
        let mut t = rt(&cur[0]);
        t.dest_path = bad.into();
        assert!(validate(&req(vec![t]), &b).is_err(), "{bad}");
    }
    // 同じ dest_path_key（大小文字違い）の 2 曲
    let cur2 = vec![item(1, "a/X.m4a", "t1"), item(2, "a/x.m4a", "t2")];
    let b2 = Basis {
        current: &cur2,
        ..b
    };
    assert!(validate(&req(cur2.iter().map(rt).collect()), &b2).is_err());
    // 同じ track_id の 2 行
    assert!(validate(&req(vec![rt(&cur[0]), rt(&cur[0])]), &b).is_err());
}

#[test]
fn delete_in_plan_does_not_allow_other_values() {
    let cur = vec![item(1, "a/1.m4a", "t1")];
    let p = plan(vec![PlanItem {
        op_id: "d".into(),
        op: K::Delete,
        track_id: 1,
        from: Some("a/1.m4a".into()),
        to: None,
        token: None,
        size: 0,
        sha256: None,
    }]);
    let b = Basis {
        desired: &[],
        desired_playlists: &[],
        current: &cur,
        current_playlists: &[],
        plan: &p,
    };
    assert!(validate(&req(vec![]), &b).is_ok());
    assert!(
        validate(&req(vec![rt(&cur[0])]), &b).is_ok(),
        "削除できなかった（保持）"
    );
}

#[test]
fn playlists_match_by_name_key_and_token() {
    use spindle::domain::device::PlaylistState;
    let cur = vec![PlaylistState {
        playlist_id: 5,
        dest_path: "Drive".into(),
        token: "pt".into(),
    }];
    let p = plan(vec![]);
    let b = Basis {
        desired: &[],
        desired_playlists: &[],
        current: &[],
        current_playlists: &cur,
        plan: &p,
    };
    let mut r = req(vec![]);
    r.state.playlists = vec![ReportPlaylist {
        playlist_id: 5,
        name: "drive".into(),
        token: "pt".into(),
    }];
    let ok = validate(&r, &b).unwrap();
    assert_eq!(ok.playlists[0].dest_path, "drive");
    r.state.playlists[0].token = "other".into();
    assert!(validate(&r, &b).is_err());
}

#[test]
fn errors_are_checked_and_digest_is_order_independent() {
    let p = plan(vec![]);
    let b = Basis {
        desired: &[],
        desired_playlists: &[],
        current: &[],
        current_playlists: &[],
        plan: &p,
    };
    let mut r = req(vec![]);
    r.errors = vec![ReportError {
        kind: ErrorKind::Track,
        ref_id: 1,
        reason: "".into(),
    }];
    assert!(validate(&r, &b).is_err(), "空の理由");
    r.errors[0].reason = "あ".repeat(1_001);
    assert!(validate(&r, &b).is_err(), "長すぎる理由");
    let e1 = ReportError {
        kind: ErrorKind::Track,
        ref_id: 2,
        reason: "x".into(),
    };
    let e2 = ReportError {
        kind: ErrorKind::Playlist,
        ref_id: 1,
        reason: "y".into(),
    };
    let mut a = req(vec![]);
    a.errors = vec![e1.clone(), e2.clone()];
    let mut c = req(vec![]);
    c.errors = vec![e2, e1];
    assert_eq!(digest(&a), digest(&c));
    c.generation = 2;
    assert_ne!(digest(&a), digest(&c));
}

fn stale(id: i64, path: &str) -> ReportTrack {
    ReportTrack {
        track_id: id,
        dest_path: path.into(),
        token: String::new(),
        size: 9,
        sha256: "x".into(),
    }
}

#[test]
fn stale_track_on_current_path_is_accepted() {
    let cur = vec![item(1, "a.m4a", "t1")];
    let p = plan(vec![]);
    let b = Basis {
        desired: &[],
        desired_playlists: &[],
        current: &cur,
        current_playlists: &[],
        plan: &p,
    };
    let ok = validate(&req(vec![stale(1, "a.m4a")]), &b).unwrap();
    assert_eq!(ok.items[0].token, "");
}

#[test]
fn stale_track_on_unknown_path_is_rejected() {
    let cur = vec![item(1, "a.m4a", "t1")];
    let p = plan(vec![]);
    let b = Basis {
        desired: &[],
        desired_playlists: &[],
        current: &cur,
        current_playlists: &[],
        plan: &p,
    };
    assert!(validate(&req(vec![stale(1, "elsewhere.m4a")]), &b).is_err());
}

#[test]
fn stale_track_for_unknown_id_is_rejected() {
    let cur = vec![item(1, "a.m4a", "t1")];
    let p = plan(vec![]);
    let b = Basis {
        desired: &[],
        desired_playlists: &[],
        current: &cur,
        current_playlists: &[],
        plan: &p,
    };
    assert!(validate(&req(vec![stale(2, "a.m4a")]), &b).is_err());
}

#[test]
fn stale_track_on_plan_destination_is_accepted() {
    let cur = vec![item(1, "a.m4a", "t1")];
    let p = plan(vec![PlanItem {
        op_id: "m".into(),
        op: K::Move,
        track_id: 1,
        from: Some("a.m4a".into()),
        to: Some("b.m4a".into()),
        token: Some("t1".into()),
        size: 10,
        sha256: Some("s1".into()),
    }]);
    let b = Basis {
        desired: &[],
        desired_playlists: &[],
        current: &cur,
        current_playlists: &[],
        plan: &p,
    };
    assert!(validate(&req(vec![stale(1, "b.m4a")]), &b).is_ok());
}
