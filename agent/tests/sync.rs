#![cfg(feature = "fake")]
mod support;

use spindle_agent::state::STALE_TOKEN;
use spindle_agent::sync::SyncOutcome;
use spindle_agent::Error;
use support::Env;

#[test]
fn first_sync_adds_tracks_and_playlist_and_reports() {
    let mut env = Env::new();
    let folder = env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.server.put_track(2, "A/b.m4a", b"bbb");
    env.server.put_playlist(10, "Favs", &[2, 1]);
    let out = env.sync().unwrap();
    assert!(
        matches!(
            out,
            SyncOutcome::Applied {
                executed: 3,
                errors: 0,
                ..
            }
        ),
        "{out:?}"
    );
    assert_eq!(env.server.reports().len(), 1);
    assert!(env.server.open().is_none());
    assert_eq!(env.server.current().len(), 2);
    let s = env.state();
    assert_eq!(s.plan_id, None);
    assert!(!s.needs_report);
    assert_eq!(
        env.music
            .playlists()
            .iter()
            .filter(|p| p.parent.as_deref() == Some(&folder))
            .count(),
        1
    );
    // 2 回目は何もしない
    assert_eq!(env.sync().unwrap(), SyncOutcome::NothingToDo);
}

#[test]
fn declined_does_not_confirm() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.ui.answers.push_back(false);
    assert_eq!(env.sync().unwrap(), SyncOutcome::Declined);
    assert!(env.server.open().is_none());
    assert!(env.music.tracks().is_empty());
}

#[test]
fn pending_reevaluation_shows_but_does_not_apply() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.server.set_pending_reevaluation(true);
    assert_eq!(env.sync().unwrap(), SyncOutcome::PendingReevaluation);
    assert!(env.music.tracks().is_empty());
}

#[test]
fn manual_deletion_is_reported_first_then_readded() {
    let mut env = Env::new();
    env.paired();
    let e = env.synced_track(1, "A/a.m4a", b"aaa");
    env.music.remove_track(&e.persistent_id);
    let out = env.sync().unwrap();
    // 報告だけの回で current から外れ、その後の差分で追加として戻る
    assert!(
        matches!(out, SyncOutcome::Applied { executed: 1, .. }),
        "{out:?}"
    );
    assert_eq!(env.server.reports().len(), 2);
    assert!(env.server.reports()[0].state.tracks.is_empty());
    assert_eq!(env.music_paths().len(), 1);
}

#[test]
fn stale_copy_is_reported_with_empty_token_and_updated() {
    let mut env = Env::new();
    env.paired();
    let e = env.synced_track(1, "A/a.m4a", b"aaa");
    env.write_local("A/a.m4a", b"edited");
    let out = env.sync().unwrap();
    assert!(
        matches!(out, SyncOutcome::Applied { executed: 1, .. }),
        "{out:?}"
    );
    assert_eq!(env.server.reports()[0].state.tracks[0].token, STALE_TOKEN);
    assert_eq!(
        std::fs::read(env.root.abs("A/a.m4a").unwrap()).unwrap(),
        b"aaa"
    );
    assert_eq!(env.music.tracks()[0].persistent_id, e.persistent_id);
}

#[test]
fn resume_skips_changed_ops() {
    let mut env = Env::new();
    env.paired();
    env.synced_track(1, "A/a.m4a", b"aaa");
    env.server.put_track(2, "A/b.m4a", b"bbb");
    env.server.put_track(3, "A/c.m4a", b"ccc");
    // 追加 2 の途中（ファイルを置いた直後）で落とす
    env.fp.arm("exec.add.placed", 1);
    assert!(matches!(env.sync(), Err(Error::Crash(_))));
    // その間にサーバ側が変わった: 3 の中身が変わり、1 が選曲から外れた（計画に無い新しい削除）
    env.server.put_track(3, "A/c.m4a", b"c2");
    env.server.remove_track(1);
    env.ui.answers.push_back(false); // 再開の後の新しい差分は断る
    assert_eq!(env.sync().unwrap(), SyncOutcome::Declined);
    // 再開は 2 だけを実行し、3 の古い追加と 1 の削除は実行しない
    let paths: Vec<_> = env.music_paths().into_iter().map(|(p, _)| p).collect();
    assert_eq!(paths, vec!["A/a.m4a", "A/b.m4a"]);
    // 再開の報告で計画は閉じ、残りは新しい差分として表示された
    assert!(env.server.open().is_none());
    let shown = env.ui.shown.last().unwrap();
    assert!(shown
        .diff
        .items
        .iter()
        .any(|o| o.track_id == 1 && o.op == agent_proto::OpKind::Delete));
}

#[test]
fn whole_sync_survives_every_state_save_crash() {
    let mut crashed = 0;
    for nth in 1..=40 {
        let mut env = Env::new();
        let folder = env.paired();
        env.synced_track(1, "x.m4a", b"one");
        env.synced_track(2, "y.m4a", b"two");
        env.synced_track(3, "gone.m4a", b"bye");
        env.server.put_track(1, "y.m4a", b"one");
        env.server.put_track(2, "x.m4a", b"two-new");
        env.server.remove_track(3);
        env.server.put_track(4, "new.m4a", b"four");
        env.server.put_playlist(10, "Favs", &[4, 1]);
        env.fp.arm("state.saved", nth);
        let first = env.sync();
        if first.is_ok() {
            break; // 保存の回数を超えた
        }
        crashed += 1;
        assert!(matches!(first, Err(Error::Crash(_))), "{nth}: {first:?}");
        let second = env.sync();
        assert!(second.is_ok(), "{nth}: {second:?}");
        assert_eq!(env.sync().unwrap(), SyncOutcome::NothingToDo, "{nth}");
        let paths: Vec<_> = env.music_paths().into_iter().map(|(p, _)| p).collect();
        assert_eq!(paths, vec!["new.m4a", "x.m4a", "y.m4a"], "{nth}");
        assert_eq!(
            std::fs::read(env.root.abs("x.m4a").unwrap()).unwrap(),
            b"two-new",
            "{nth}"
        );
        let lists: Vec<_> = env
            .music
            .playlists()
            .into_iter()
            .filter(|p| p.parent.as_deref() == Some(&folder))
            .collect();
        assert_eq!(lists.len(), 1, "{nth}");
        assert_eq!(lists[0].name, "Favs", "{nth}");
        let s = env.state();
        assert!(
            s.pending_ops.is_empty() && s.pending_batches.is_empty() && !s.needs_report,
            "{nth}"
        );
    }
    eprintln!("state.saved で落とした回数: {crashed}");
    assert!(crashed > 5, "保存の回数が少なすぎる: {crashed}");
    assert!(crashed < 40, "40 回では保存を数え尽くせない");
}

#[test]
fn marker_mismatch_stops_before_anything() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    std::fs::remove_file(env.root.path().join(".spindle-device")).unwrap();
    assert!(matches!(env.sync(), Err(Error::Stop(_))));
    assert!(env.server.open().is_none());
}

#[test]
fn unpaired_sync_stops() {
    let mut env = Env::new();
    assert!(matches!(env.sync(), Err(Error::Stop(m)) if m.contains("pair")));
}

#[test]
fn generation_change_before_report_stops() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.fp.arm("music.add:after", 1);
    assert!(env.sync().is_err());
    env.server.bump_generation();
    // 再開の報告が generation の不一致で拒否される
    assert!(matches!(env.sync(), Err(Error::Stop(m)) if m.contains("設定")));
}

#[test]
fn abandon_requires_no_pending_and_closes_plan() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.fp.arm("exec.add.placed", 1);
    assert!(env.sync().is_err());
    let paths = env.paths();
    let r = spindle_agent::sync::abandon(&env.music, &env.server, &paths, &env.fp, &mut env.ui);
    assert!(
        matches!(r, Err(Error::Stop(_))),
        "pending が残っているので拒否"
    );
    // 回復だけを走らせて pending を片付ける（計画は open のまま）
    env.with_ctx(spindle_agent::recover::recover).unwrap();
    assert!(env.server.open().is_some());
    spindle_agent::sync::abandon(&env.music, &env.server, &paths, &env.fp, &mut env.ui).unwrap();
    assert!(env.server.open().is_none());
    assert_eq!(env.server.abandons().len(), 1);
    assert_eq!(env.state().plan_id, None);
}

#[test]
fn abandon_without_open_plan_is_noop() {
    let mut env = Env::new();
    env.paired();
    let paths = env.paths();
    spindle_agent::sync::abandon(&env.music, &env.server, &paths, &env.fp, &mut env.ui).unwrap();
    assert!(env.server.abandons().is_empty());
}

#[test]
fn status_mentions_counts() {
    let env = Env::new();
    env.paired();
    env.synced_track(1, "A/a.m4a", b"aaa");
    let s = spindle_agent::sync::status(&env.paths()).unwrap();
    assert!(s.contains("曲: 1"), "{s}");
    assert!(s.contains("iPhone"), "{s}");
}

#[test]
fn status_without_pair_stops() {
    let env = Env::new();
    assert!(
        matches!(spindle_agent::sync::status(&env.paths()), Err(Error::Stop(m)) if m.contains("pair"))
    );
}

#[test]
fn render_diff_summarizes_and_truncates() {
    let env = Env::new();
    for i in 0..60 {
        env.server.put_track(i, &format!("A/{i:02}.m4a"), b"x");
    }
    let m = spindle_agent::server::Server::manifest(&env.server).unwrap();
    let s = spindle_agent::sync::render_diff(&m);
    assert!(s.contains("追加 60"), "{s}");
    assert!(s.contains("A/00.m4a"), "{s}");
    assert!(!s.contains("A/59.m4a"), "{s}");
    assert!(s.contains("ほか 10 件"), "{s}");
}

#[test]
fn plan_confirmed_by_failed_report_only_round_is_closed_without_executing() {
    let mut env = Env::new();
    env.paired();
    let e = env.synced_track(1, "A/a.m4a", b"aaa");
    env.music.remove_track(&e.persistent_id);
    env.server.put_track(2, "A/b.m4a", b"bbb");
    // 報告だけの回: 確定（add 2 を含む計画）の後、報告が届かずに終わる
    env.server.fail_report(1);
    assert!(matches!(env.sync(), Err(Error::Server(_))));
    assert!(env.server.open().is_some());
    assert!(env.state().needs_report);
    assert_eq!(env.state().plan_id, None);
    // 次の sync は y/N の前に何も実行しない。断れば何も変わらない
    env.ui.answers.push_back(false);
    assert_eq!(env.sync().unwrap(), SyncOutcome::Declined);
    assert!(env.music.tracks().is_empty());
    assert!(!env.root.exists("A/b.m4a").unwrap());
    assert!(env.state().tracks.is_empty());
    assert!(env.server.open().is_none());
    assert_eq!(env.server.reports().len(), 1);
    assert!(env.server.reports()[0].state.tracks.is_empty());
    assert!(env
        .ui
        .log
        .iter()
        .any(|l| l.contains("実行せずに閉じました")));
    let shown = env.ui.shown.last().unwrap();
    assert_eq!(shown.diff.items.len(), 2, "{:?}", shown.diff.items);
}

#[test]
fn plan_confirmed_but_not_recorded_is_closed_without_executing() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    // 確定だけされて plan_id を記録する前に終わった計画
    let m = spindle_agent::server::Server::manifest(&env.server).unwrap();
    let c = spindle_agent::server::Server::confirm(&env.server, &m.plan_token).unwrap();
    assert!(matches!(c, spindle_agent::server::Confirmed::Plan(_)));
    env.ui.answers.push_back(false);
    assert_eq!(env.sync().unwrap(), SyncOutcome::Declined);
    assert!(env.music.tracks().is_empty());
    assert!(!env.root.exists("A/a.m4a").unwrap());
    assert!(env.server.open().is_none());
    assert_eq!(env.server.reports().len(), 1);
    assert_eq!(env.ui.shown.len(), 1);
}

#[test]
fn report_only_round_waits_for_reevaluation() {
    let mut env = Env::new();
    env.paired();
    let e = env.synced_track(1, "A/a.m4a", b"aaa");
    env.music.remove_track(&e.persistent_id);
    env.server.set_pending_reevaluation(true);
    assert_eq!(env.sync().unwrap(), SyncOutcome::PendingReevaluation);
    assert!(env.state().needs_report);
    assert!(env.server.reports().is_empty());
    assert!(env.server.open().is_none());
    assert!(env.ui.shown.is_empty());
}

/// 実行の後に残った差分（保留にした操作など）は件数を知らせる。残りが無ければ知らせない
#[test]
fn remaining_diff_after_apply_is_noted() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.server.put_track(2, "A/b.m4a", b"bbb");
    env.write_local("A/a.m4a", b"mine");
    let out = env.sync().unwrap();
    assert!(
        matches!(out, SyncOutcome::Applied { errors: 1, .. }),
        "{out:?}"
    );
    assert!(
        env.ui
            .log
            .iter()
            .any(|l| l.contains("1 件") && l.contains("次の sync")),
        "{:?}",
        env.ui.log
    );

    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    env.sync().unwrap();
    assert!(
        env.ui.log.iter().all(|l| !l.contains("次の sync")),
        "{:?}",
        env.ui.log
    );
}

#[test]
fn media_folder_root_stops_sync_without_touching_music() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    std::fs::create_dir(env.root.path().join("Automatically Add to Music.localized")).unwrap();
    let calls = env.music.calls().len();
    assert!(matches!(env.sync(), Err(Error::Stop(m)) if m.contains("メディアフォルダ")));
    assert!(env.server.open().is_none());
    assert!(env.music.tracks().is_empty());
    assert_eq!(env.music.calls().len(), calls);
}

#[test]
fn media_folder_parent_stops_sync_without_touching_music() {
    let mut env = Env::new();
    env.paired();
    env.server.put_track(1, "A/a.m4a", b"aaa");
    let parent = env.root.path().parent().unwrap().to_path_buf();
    std::fs::create_dir(parent.join("Automatically Add to Music.localized")).unwrap();
    let calls = env.music.calls().len();
    assert!(matches!(env.sync(), Err(Error::Stop(m)) if m.contains("メディアフォルダ")));
    assert!(env.server.open().is_none());
    assert!(env.music.tracks().is_empty());
    assert_eq!(env.music.calls().len(), calls);
}
