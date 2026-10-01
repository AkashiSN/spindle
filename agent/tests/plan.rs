//! 計画の再開の照合（本体の tests/device_plan.rs の移植）

use std::collections::HashSet;

use agent_proto::*;
use spindle_agent::plan::*;

fn op(kind: OpKind, id: i64, from: Option<&str>, to: Option<&str>, token: Option<&str>) -> ItemOp {
    ItemOp {
        op: kind,
        track_id: id,
        from: from.map(Into::into),
        to: to.map(Into::into),
        token: token.map(Into::into),
        size: 1,
        sha256: token.map(|t| format!("h-{t}")),
    }
}

fn cur(id: i64, path: &str, token: &str) -> Current {
    Current {
        track_id: id,
        path: path.into(),
        token: token.into(),
    }
}

fn diff(items: Vec<ItemOp>) -> DiffView {
    DiffView {
        items,
        held: vec![],
        playlists: vec![],
        playlist_errors: vec![],
    }
}

fn plan_of(d: &DiffView) -> Plan {
    let mut n = 0;
    let mut id = || {
        n += 1;
        format!("{n:032x}")
    };
    Plan {
        plan_id: 1,
        generation: 3,
        plan_token: "tok".into(),
        items: d
            .items
            .iter()
            .map(|o| PlanItem {
                op_id: id(),
                op: o.op,
                track_id: o.track_id,
                from: o.from.clone(),
                to: o.to.clone(),
                token: o.token.clone(),
                size: o.size,
                sha256: o.sha256.clone(),
            })
            .collect(),
        playlists: d
            .playlists
            .iter()
            .map(|p| PlanPlaylist {
                op_id: id(),
                op: p.op,
                playlist_id: p.playlist_id,
                from: p.from.clone(),
                to: p.to.clone(),
                token: p.token.clone(),
            })
            .collect(),
    }
}

fn pl_diff(playlists: Vec<PlaylistOp>) -> DiffView {
    DiffView {
        items: vec![],
        held: vec![],
        playlists,
        playlist_errors: vec![],
    }
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
    let d = pl_diff(vec![
        PlaylistOp {
            op: PlaylistOpKind::Add,
            playlist_id: 1,
            from: None,
            to: Some("Playlists/a.m3u8".into()),
            token: Some("p1".into()),
        },
        PlaylistOp {
            op: PlaylistOpKind::Delete,
            playlist_id: 2,
            from: Some("Playlists/b.m3u8".into()),
            to: None,
            token: None,
        },
    ]);
    let p = plan_of(&d);
    let current = [
        CurrentPlaylist {
            playlist_id: 1,
            name: "Playlists/a.m3u8".into(),
            token: "p1".into(),
        },
        CurrentPlaylist {
            playlist_id: 2,
            name: "Playlists/b.m3u8".into(),
            token: "p2".into(),
        },
    ];
    // 1 は済み、2 は今の差分から消えた
    let r = runnable(&p, &[], &current, &diff(vec![]));
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
        op: kind,
        playlist_id: id,
        from: from.map(Into::into),
        to: to.map(Into::into),
        token: token.map(Into::into),
    }
}

#[test]
fn rename_cut_after_removing_the_old_path_resumes_as_add() {
    let d = pl_diff(vec![pl_op(
        PlaylistOpKind::Update,
        5,
        Some("Playlists/old.m3u8"),
        Some("Playlists/new.m3u8"),
        Some("p2"),
    )]);
    let p = plan_of(&d);
    // 旧パスを消した後・新パスを書く前に切れた。今の状態に 5 の行が無く、今の差分は追加
    let now = pl_diff(vec![pl_op(
        PlaylistOpKind::Add,
        5,
        None,
        Some("Playlists/new.m3u8"),
        Some("p2"),
    )]);
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
        let now = pl_diff(vec![now]);
        let r = runnable(&p, &[], &[], &now);
        assert!(r.playlists.is_empty());
        assert_eq!(r.dropped, 1);
    }
    let still = CurrentPlaylist {
        playlist_id: 5,
        name: "Playlists/old.m3u8".into(),
        token: "p1".into(),
    };
    let r = runnable(&p, &[], &[still], &now);
    assert!(r.playlists.is_empty());
    assert_eq!(r.dropped, 1);
}

#[test]
fn current_of_reads_state() {
    use spindle_agent::state::*;
    let mut s = State::default();
    s.tracks.insert(
        2,
        TrackEntry {
            persistent_id: "P".into(),
            token: "t".into(),
            path: "a.m4a".into(),
            size: 1,
            sha256: "h".into(),
            inode: 1,
            mtime_ns: 1,
        },
    );
    s.playlists.insert(
        9,
        PlaylistEntry {
            persistent_id: "Q".into(),
            name: "Favs".into(),
            token: "p".into(),
        },
    );
    let (c, p) = current_of(&s);
    assert_eq!(c, vec![cur(2, "a.m4a", "t")]);
    assert_eq!(
        p,
        vec![CurrentPlaylist {
            playlist_id: 9,
            name: "Favs".into(),
            token: "p".into()
        }]
    );
}

#[test]
fn drop_components_removes_whole_cycle() {
    let d = diff(vec![
        op(OpKind::Move, 1, Some("a"), Some("b"), Some("t")),
        op(OpKind::Move, 2, Some("b"), Some("a"), Some("t")),
        op(OpKind::Move, 3, Some("c"), Some("d"), Some("t")),
    ]);
    let p = plan_of(&d);
    let left = drop_components(p.items.clone(), &HashSet::from([2]));
    assert_eq!(left.iter().map(|o| o.track_id).collect::<Vec<_>>(), vec![3]);
}
