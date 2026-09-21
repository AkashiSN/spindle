//! 再生リストの位置 ↔ TRACKNUMBER の揃え（`import::ytmusic::playlist::plan_align`。P4-16、D-78）。
//! 純粋な計画だけ。バッチの投入と待ちは tests/playlist_sync.rs

use spindle::import::ytmusic::playlist::{
    plan_align, AlignPlan, Availability, BlockReason, LibraryRow, PlaylistEntry,
};

fn entry(position: u32, id: &str) -> PlaylistEntry {
    PlaylistEntry {
        position,
        id: id.to_owned(),
        url: format!("https://www.youtube.com/watch?v={id}"),
        title: Some(format!("song {id}")),
        availability: Availability::Available,
    }
}

fn row(track_id: i64, track_no: Option<i64>, id: Option<&str>) -> LibraryRow {
    LibraryRow {
        track_id,
        disc_no: Some(1),
        track_no,
        source_url: id.map(|id| format!("https://www.youtube.com/watch?v={id}")),
    }
}

fn moves(p: &AlignPlan) -> Vec<(i64, i64)> {
    p.moves.iter().map(|m| (m.track_id, m.target_no)).collect()
}

#[test]
fn matching_rows_are_unchanged_and_shifted_rows_move_to_their_position() {
    let entries = [entry(1, "a"), entry(2, "b"), entry(3, "c")];
    // b が 5 番になっている
    let rows = [
        row(10, Some(1), Some("a")),
        row(11, Some(5), Some("b")),
        row(12, Some(3), Some("c")),
    ];
    let p = plan_align(&entries, &rows);
    assert_eq!(moves(&p), [(11, 2)]);
    assert_eq!(p.unchanged, 2);
    assert!(p.blocked.is_empty());
    assert_eq!(p.missing, Vec::<u32>::new(), "全部 Library にある");
}

#[test]
fn unavailable_and_missing_entries_keep_their_positions_so_numbers_skip() {
    let mut entries = vec![entry(1, "a"), entry(2, "p"), entry(3, "c"), entry(4, "d")];
    entries[1].availability =
        Availability::Unavailable(spindle::import::ytmusic::playlist::UnavailableKind::Private);
    entries[1].title = None;
    // Library には a, c, d が 1, 2, 3 で並んでいる（p は非公開で無い、d は未取り込みだった頃の番号）
    let rows = [
        row(10, Some(1), Some("a")),
        row(12, Some(2), Some("c")),
        row(13, Some(3), Some("d")),
    ];
    let p = plan_align(&entries, &rows);
    assert_eq!(moves(&p), [(12, 3), (13, 4)]);
    assert_eq!(
        p.missing,
        [2],
        "非公開の 2 番は Library に無い（番号が飛ぶ）"
    );
}

#[test]
fn swap_and_cycle_among_targets_are_allowed() {
    let entries = [entry(1, "a"), entry(2, "b"), entry(3, "c")];
    // 3-cycle: a→2, b→3, c→1 の位置にいる
    let rows = [
        row(10, Some(2), Some("a")),
        row(11, Some(3), Some("b")),
        row(12, Some(1), Some("c")),
    ];
    let p = plan_align(&entries, &rows);
    assert_eq!(moves(&p), [(10, 1), (11, 2), (12, 3)]);
    assert!(p.blocked.is_empty());
}

#[test]
fn rows_without_source_url_or_outside_the_playlist_are_fixed_and_block_their_numbers() {
    let entries = [entry(1, "a"), entry(2, "b"), entry(3, "c")];
    let rows = [
        row(10, Some(1), Some("a")),
        // 2 番は SOURCE_URL 無しの行が使っている
        row(20, Some(2), None),
        row(11, Some(4), Some("b")),
        // 3 番は再生リストに無い動画の行が使っている
        row(21, Some(3), Some("zzz")),
        row(12, Some(5), Some("c")),
    ];
    let p = plan_align(&entries, &rows);
    assert!(moves(&p).is_empty());
    let blocked: Vec<(i64, u32, BlockReason)> = p
        .blocked
        .iter()
        .map(|b| (b.track_id, b.position, b.reason.clone()))
        .collect();
    assert_eq!(
        blocked,
        [
            (11, 2, BlockReason::NumberTaken { by_track_id: 20 }),
            (12, 3, BlockReason::NumberTaken { by_track_id: 21 }),
        ]
    );
    assert_eq!(p.outsiders, 1, "SOURCE_URL 付きだが再生リストに無い行");
    assert_eq!(p.unnumbered, 1, "SOURCE_URL の無い行");
}

#[test]
fn duplicate_source_url_rows_and_other_discs_are_not_moved() {
    let entries = [entry(1, "a"), entry(2, "b"), entry(3, "c")];
    let mut disc2 = row(12, Some(9), Some("c"));
    disc2.disc_no = Some(2);
    let rows = [
        row(10, Some(7), Some("a")),
        row(30, Some(8), Some("a")),
        row(11, Some(2), Some("b")),
        disc2,
    ];
    let p = plan_align(&entries, &rows);
    assert!(moves(&p).is_empty());
    let reasons: Vec<(i64, BlockReason)> = p
        .blocked
        .iter()
        .map(|b| (b.track_id, b.reason.clone()))
        .collect();
    assert_eq!(
        reasons,
        [
            (10, BlockReason::DuplicateSourceUrl),
            (30, BlockReason::DuplicateSourceUrl),
            (12, BlockReason::OtherDisc { disc_no: 2 }),
        ]
    );
    assert_eq!(p.unchanged, 1);
}

#[test]
fn a_target_can_take_a_number_freed_by_another_target_but_not_one_held_by_a_fixed_row_on_disc_one_only(
) {
    let entries = [entry(1, "a"), entry(2, "b")];
    // 固定行は disc 2 の 1 番: disc 1 の番号とは重ならないので塞がない
    let mut fixed = row(20, Some(1), None);
    fixed.disc_no = Some(2);
    let rows = [
        row(10, Some(2), Some("a")),
        row(11, Some(1), Some("b")),
        fixed,
    ];
    let p = plan_align(&entries, &rows);
    assert_eq!(moves(&p), [(10, 1), (11, 2)]);
    // NULL の disc は 1 とみなす
    let mut null_disc = row(10, Some(2), Some("a"));
    null_disc.disc_no = None;
    let p = plan_align(&entries, &[null_disc, row(11, Some(1), Some("b"))]);
    assert_eq!(moves(&p), [(10, 1), (11, 2)]);
}

#[test]
fn rows_without_a_number_are_moved_when_they_match() {
    let entries = [entry(1, "a")];
    let p = plan_align(&entries, &[row(10, None, Some("a"))]);
    assert_eq!(moves(&p), [(10, 1)]);
}
