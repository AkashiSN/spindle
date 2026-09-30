//! 確定した計画と再開（仕様 ③「開始済みの計画の再開」）

use spindle::device::plan::*;
use spindle::domain::device::*;

fn op(kind: OpKind, id: i64, from: Option<&str>, to: Option<&str>, token: Option<&str>) -> ItemOp {
    ItemOp {
        kind,
        track_id: id,
        from: from.map(Into::into),
        to: to.map(Into::into),
        token: token.map(Into::into),
        size: 1,
        sha256: token.map(|t| format!("h-{t}")),
    }
}

fn cur(id: i64, path: &str, token: &str) -> DeviceItem {
    DeviceItem {
        track_id: id,
        dest_path: path.into(),
        token: token.into(),
        size: 1,
        sha256: format!("h-{token}"),
    }
}

fn diff(items: Vec<ItemOp>) -> Diff {
    Diff {
        items,
        ..Default::default()
    }
}

fn plan_of(d: &Diff) -> StoredPlan {
    StoredPlan::from_diff(3, "tok", d).unwrap()
}

#[test]
fn from_diff_assigns_unique_op_ids_and_keeps_ops() {
    let d = diff(vec![
        op(OpKind::Delete, 1, Some("a.opus"), None, None),
        op(OpKind::Add, 2, None, Some("b.opus"), Some("t2")),
    ]);
    let p = plan_of(&d);
    assert_eq!(p.v, PLAN_VERSION);
    assert_eq!(p.generation, 3);
    assert_eq!(p.plan_token, "tok");
    assert_eq!(p.items.len(), 2);
    assert_ne!(p.items[0].op_id, p.items[1].op_id);
    assert_eq!(p.items[1].op, OpKind::Add);
    assert_eq!(p.items[1].sha256.as_deref(), Some("h-t2"));
    let json = serde_json::to_string(&p).unwrap();
    assert_eq!(serde_json::from_str::<StoredPlan>(&json).unwrap(), p);
}

#[test]
fn unchanged_plan_runs_everything() {
    let d = diff(vec![
        op(OpKind::Delete, 1, Some("a.opus"), None, None),
        op(OpKind::Add, 2, None, Some("b.opus"), Some("t2")),
    ]);
    let p = plan_of(&d);
    let r = runnable(&p, &[cur(1, "a.opus", "t1")], &[], &d);
    assert_eq!(r.items.len(), 2);
    assert_eq!(r.satisfied, 0);
    assert_eq!(r.dropped, 0);
}

#[test]
fn satisfied_ops_are_skipped() {
    let d = diff(vec![
        op(OpKind::Delete, 1, Some("a.opus"), None, None),
        op(OpKind::Add, 2, None, Some("b.opus"), Some("t2")),
    ]);
    let p = plan_of(&d);
    // 途中で落ちて、削除と追加が済んでいる
    let now = diff(vec![]);
    let r = runnable(&p, &[cur(2, "b.opus", "t2")], &[], &now);
    assert!(r.items.is_empty());
    assert_eq!(r.satisfied, 2);
}

#[test]
fn new_delete_is_not_run() {
    let d = diff(vec![op(OpKind::Add, 2, None, Some("b.opus"), Some("t2"))]);
    let p = plan_of(&d);
    let now = diff(vec![
        op(OpKind::Delete, 9, Some("z.opus"), None, None),
        op(OpKind::Add, 2, None, Some("b.opus"), Some("t2")),
    ]);
    let r = runnable(&p, &[cur(9, "z.opus", "t9")], &[], &now);
    assert_eq!(r.items.len(), 1);
    assert_eq!(r.items[0].track_id, 2);
}

#[test]
fn changed_token_or_path_is_dropped() {
    let d = diff(vec![
        op(OpKind::Update, 1, None, Some("a.opus"), Some("t1b")),
        op(OpKind::Add, 2, None, Some("b.opus"), Some("t2")),
    ]);
    let p = plan_of(&d);
    let now = diff(vec![
        op(OpKind::Update, 1, None, Some("a.opus"), Some("t1c")),
        op(OpKind::Add, 2, None, Some("b2.opus"), Some("t2")),
    ]);
    let r = runnable(&p, &[cur(1, "a.opus", "t1")], &[], &now);
    assert!(r.items.is_empty());
    assert_eq!(r.dropped, 2);
}

#[test]
fn stale_member_drops_whole_swap_component() {
    // x ↔ y の入れ替えと、無関係な移動 p → q
    let d = diff(vec![
        op(OpKind::Move, 1, Some("x.opus"), Some("y.opus"), Some("t1")),
        op(OpKind::Move, 2, Some("y.opus"), Some("x.opus"), Some("t2")),
        op(OpKind::Move, 3, Some("p.opus"), Some("q.opus"), Some("t3")),
    ]);
    let p = plan_of(&d);
    // 2 の行き先が変わった
    let now = diff(vec![
        op(OpKind::Move, 1, Some("x.opus"), Some("y.opus"), Some("t1")),
        op(OpKind::Move, 2, Some("y.opus"), Some("w.opus"), Some("t2")),
        op(OpKind::Move, 3, Some("p.opus"), Some("q.opus"), Some("t3")),
    ]);
    let current = [
        cur(1, "x.opus", "t1"),
        cur(2, "y.opus", "t2"),
        cur(3, "p.opus", "t3"),
    ];
    let r = runnable(&p, &current, &[], &now);
    assert_eq!(
        r.items.iter().map(|i| i.track_id).collect::<Vec<_>>(),
        vec![3]
    );
    assert_eq!(r.dropped, 2);
}

#[test]
fn components_follow_chains_and_cycles() {
    let d = diff(vec![
        op(OpKind::Move, 1, Some("a"), Some("b"), Some("t")),
        op(OpKind::UpdateMove, 2, Some("b"), Some("c"), Some("t")),
        op(OpKind::Move, 3, Some("c"), Some("a"), Some("t")),
        op(OpKind::Move, 4, Some("d"), Some("e"), Some("t")),
        op(OpKind::Update, 5, None, Some("f"), Some("t")),
    ]);
    let p = plan_of(&d);
    let comps = path_components(&p.items);
    assert_eq!(comps, vec![vec![0, 1, 2], vec![3]]);
}

#[test]
fn case_only_rename_links_to_itself_only() {
    let d = diff(vec![
        op(OpKind::Move, 1, Some("A.opus"), Some("a.opus"), Some("t")),
        op(OpKind::Move, 2, Some("b.opus"), Some("B.opus"), Some("t")),
    ]);
    let p = plan_of(&d);
    assert_eq!(path_components(&p.items), vec![vec![0], vec![1]]);
}

#[test]
fn playlists_follow_the_same_rules() {
    let d = Diff {
        playlists: vec![
            PlaylistOp {
                kind: PlaylistOpKind::Add,
                playlist_id: 1,
                from: None,
                to: Some("Playlists/a.m3u8".into()),
                token: Some("p1".into()),
            },
            PlaylistOp {
                kind: PlaylistOpKind::Delete,
                playlist_id: 2,
                from: Some("Playlists/b.m3u8".into()),
                to: None,
                token: None,
            },
        ],
        ..Default::default()
    };
    let p = plan_of(&d);
    let current = [
        PlaylistState {
            playlist_id: 1,
            dest_path: "Playlists/a.m3u8".into(),
            token: "p1".into(),
        },
        PlaylistState {
            playlist_id: 2,
            dest_path: "Playlists/b.m3u8".into(),
            token: "p2".into(),
        },
    ];
    // 1 は済み、2 は今の差分から消えた
    let r = runnable(&p, &[], &current, &Diff::default());
    assert!(r.playlists.is_empty());
    assert_eq!(r.satisfied, 1);
    assert_eq!(r.dropped, 1);
}

fn pl_op(
    kind: PlaylistOpKind,
    id: i64,
    from: Option<&str>,
    to: Option<&str>,
    token: Option<&str>,
) -> PlaylistOp {
    PlaylistOp {
        kind,
        playlist_id: id,
        from: from.map(Into::into),
        to: to.map(Into::into),
        token: token.map(Into::into),
    }
}

#[test]
fn rename_cut_after_removing_the_old_path_resumes_as_add() {
    let d = Diff {
        playlists: vec![pl_op(
            PlaylistOpKind::Update,
            5,
            Some("Playlists/old.m3u8"),
            Some("Playlists/new.m3u8"),
            Some("p2"),
        )],
        ..Default::default()
    };
    let p = plan_of(&d);
    // 旧パスを消した後・新パスを書く前に切れた。今の状態に 5 の行が無く、今の差分は追加
    let now = Diff {
        playlists: vec![pl_op(
            PlaylistOpKind::Add,
            5,
            None,
            Some("Playlists/new.m3u8"),
            Some("p2"),
        )],
        ..Default::default()
    };
    let r = runnable(&p, &[], &[], &now);
    assert_eq!(r.dropped, 0);
    assert_eq!(
        r.playlists,
        vec![PlanPlaylist {
            op_id: p.playlists[0].op_id.clone(),
            op: PlaylistOpKind::Add,
            playlist_id: 5,
            from: None,
            to: Some("Playlists/new.m3u8".into()),
            token: Some("p2".into()),
        }]
    );
    // 行き先かトークンが違う追加、または今の状態にまだ旧パスの行があれば外す
    for now in [
        pl_op(
            PlaylistOpKind::Add,
            5,
            None,
            Some("Playlists/x.m3u8"),
            Some("p2"),
        ),
        pl_op(
            PlaylistOpKind::Add,
            5,
            None,
            Some("Playlists/new.m3u8"),
            Some("p3"),
        ),
    ] {
        let now = Diff {
            playlists: vec![now],
            ..Default::default()
        };
        let r = runnable(&p, &[], &[], &now);
        assert!(r.playlists.is_empty());
        assert_eq!(r.dropped, 1);
    }
    let still = PlaylistState {
        playlist_id: 5,
        dest_path: "Playlists/old.m3u8".into(),
        token: "p1".into(),
    };
    let r = runnable(&p, &[], &[still], &now);
    assert!(r.playlists.is_empty());
    assert_eq!(r.dropped, 1);
}
