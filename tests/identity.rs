//! 同一性解決: `(dev, inode)` → `audio_md5` → `rel_path_key` の順に、inventory 全体を入力に
//! 取って各段で候補を検証する（SPEC §6「同一性解決の優先順位」、D-26 / D-29 / D-30）。
//! 受け入れ: docs/TASKS.md P0-5 (a) (b) (c) (d) (e) (f) (g) (h) (k)

use std::cell::RefCell;
use std::collections::HashMap;

use spindle::domain::identity::{resolve, Decision, Entry, Identity, Row, Via};

const DEV: u64 = 7;

fn md5(n: u8) -> [u8; 16] {
    [n; 16]
}

/// inventory のエントリ。size / mtime / ctime は指定がなければ inode から決まる既定値
fn entry(key: &str, inode: u64) -> Entry {
    Entry {
        key: key.to_owned(),
        dev: DEV,
        inode,
        nlink: 1,
        size: 1000 + inode,
        mtime_ns: 10_000 + inode as i64,
        ctime_ns: 20_000 + inode as i64,
    }
}

/// 既存行。既定値は同じ inode の `entry` と物理属性が一致する（= 変更なし）
fn row(id: i64, key: &str, inode: u64, md5: Option<[u8; 16]>) -> Row {
    Row {
        id,
        key: key.to_owned(),
        dev: Some(DEV),
        inode: Some(inode),
        size: 1000 + inode,
        mtime_ns: 10_000 + inode as i64,
        ctime_ns: 20_000 + inode as i64,
        audio_md5: md5,
        missing: false,
    }
}

/// エントリごとの md5 を返すコールバック。呼ばれた回数も数える
struct Md5Table {
    by_key: HashMap<String, [u8; 16]>,
    calls: RefCell<Vec<String>>,
}

impl Md5Table {
    fn new(pairs: &[(&str, [u8; 16])]) -> Self {
        Self {
            by_key: pairs.iter().map(|(k, m)| ((*k).to_owned(), *m)).collect(),
            calls: RefCell::default(),
        }
    }

    fn resolve(&self, inventory: &[Entry], rows: &[Row]) -> Vec<Decision> {
        resolve(inventory, rows, &mut |i: usize| {
            self.calls.borrow_mut().push(inventory[i].key.clone());
            self.by_key.get(&inventory[i].key).copied()
        })
    }
}

fn existing(d: &Decision) -> (i64, Via, bool) {
    match &d.identity {
        Identity::Existing {
            track_id,
            via,
            changed,
            ..
        } => (*track_id, *via, *changed),
        Identity::New => panic!("Existing のはず: {d:?}"),
    }
}

fn is_new(d: &Decision) -> bool {
    matches!(d.identity, Identity::New)
}

// ---------------------------------------------------------------- inode 段

#[test]
fn unchanged_file_matches_by_inode_without_md5() {
    let inv = [entry("a/x.flac", 1)];
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let t = Md5Table::new(&[]);
    let d = t.resolve(&inv, &rows);
    assert_eq!(existing(&d[0]), (10, Via::Inode, false));
    assert!(t.calls.borrow().is_empty(), "変更なしなら md5 を読まない");
}

#[test]
fn in_place_tag_rewrite_matches_by_inode_when_md5_agrees() {
    // 受け入れ (a): size も mtime も変わった in-place 書き換え → md5 一致を要求して同一
    let mut e = entry("a/x.flac", 1);
    e.size += 77;
    e.mtime_ns += 5;
    e.ctime_ns += 5;
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let t = Md5Table::new(&[("a/x.flac", md5(1))]);
    let d = t.resolve(&[e], &rows);
    assert_eq!(existing(&d[0]), (10, Via::Inode, true));
    assert_eq!(*t.calls.borrow(), ["a/x.flac"]);
}

#[test]
fn size_or_mtime_agreement_is_enough_for_inode_match() {
    let mut e = entry("a/x.flac", 1);
    e.mtime_ns += 5; // size は一致
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let t = Md5Table::new(&[]);
    let d = t.resolve(&[e], &rows);
    assert_eq!(existing(&d[0]), (10, Via::Inode, true));
    assert!(t.calls.borrow().is_empty());
}

#[test]
fn moved_file_matches_by_inode_and_reports_changed() {
    // 受け入れ (b): 別ディレクトリへ移動（inode 不変、パスだけ変化）
    let inv = [entry("b/y.flac", 1)];
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let d = Md5Table::new(&[]).resolve(&inv, &rows);
    let (id, via, _) = existing(&d[0]);
    assert_eq!((id, via), (10, Via::Inode));
}

#[test]
fn inode_reuse_with_different_content_is_not_a_match() {
    // 受け入れ (f): 削除後に別内容のファイルが同じ inode を得た
    let mut e = entry("b/new.flac", 1);
    e.size += 1;
    e.mtime_ns += 1;
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let t = Md5Table::new(&[("b/new.flac", md5(2))]);
    let d = t.resolve(&[e], &rows);
    assert!(is_new(&d[0]));
}

#[test]
fn inode_reuse_at_same_path_falls_back_to_path_match() {
    let mut e = entry("a/x.flac", 1);
    e.size += 1;
    e.mtime_ns += 1;
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let d = Md5Table::new(&[("a/x.flac", md5(2))]).resolve(&[e], &rows);
    assert_eq!(existing(&d[0]), (10, Via::Path, true));
}

#[test]
fn inode_stage_runs_for_all_entries_before_path_stage() {
    // 行 R は a/x.flac にあった。R のファイルは b/y.flac へ rename され、a/x.flac には別の
    // 新しいファイル（inode 2）が置かれた。key 順では a/x.flac が先だが、inode 段を全件先に
    // 回すので R は b/y.flac が取り、a/x.flac は新規になる
    let inv = [entry("a/x.flac", 2), entry("b/y.flac", 1)];
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let d = Md5Table::new(&[("a/x.flac", md5(2))]).resolve(&inv, &rows);
    assert!(is_new(&d[0]));
    assert_eq!(existing(&d[1]).0, 10);
    assert_eq!(existing(&d[1]).1, Via::Inode);
}

// ---------------------------------------------------------------- audio_md5 段

#[test]
fn copy_then_delete_matches_by_md5_when_source_is_gone() {
    // 受け入れ (b) の別経路: cp + rm（inode が変わる）。元パスが inventory に無いので移動
    let inv = [entry("b/y.flac", 5)];
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let d = Md5Table::new(&[("b/y.flac", md5(1))]).resolve(&inv, &rows);
    assert_eq!(existing(&d[0]), (10, Via::AudioMd5, true));
}

#[test]
fn copy_with_source_remaining_is_new_not_move() {
    // 受け入れ (d): コピー元が残っている → 新規（duplicate_groups に現れる）
    let inv = [entry("a/x.flac", 1), entry("b/copy.flac", 5)];
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let d = Md5Table::new(&[("b/copy.flac", md5(1))]).resolve(&inv, &rows);
    assert_eq!(existing(&d[0]), (10, Via::Inode, false));
    assert!(is_new(&d[1]));
}

#[test]
fn two_rows_with_same_md5_are_never_auto_merged() {
    // 受け入れ (e): 候補が 2 行あれば移動と判定しない
    let inv = [entry("c/z.flac", 9)];
    let rows = [
        row(10, "a/x.flac", 1, Some(md5(1))),
        row(11, "b/y.flac", 2, Some(md5(1))),
    ];
    let d = Md5Table::new(&[("c/z.flac", md5(1))]).resolve(&inv, &rows);
    assert!(is_new(&d[0]));
}

#[test]
fn md5_contention_is_settled_by_key_order() {
    // 同じ md5 の新パスが 2 つ、旧パスは消えている → key 昇順で先が移動、後は新規
    let inv = [entry("z/second.flac", 6), entry("b/first.flac", 5)];
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let d =
        Md5Table::new(&[("z/second.flac", md5(1)), ("b/first.flac", md5(1))]).resolve(&inv, &rows);
    assert!(is_new(&d[0]));
    assert_eq!(existing(&d[1]), (10, Via::AudioMd5, true));
}

#[test]
fn md5_stage_is_skipped_when_entry_has_no_md5() {
    // 受け入れ (c): MD5 未設定の FLAC / 非可逆は md5 段を素通りして落ちない
    let inv = [entry("b/y.flac", 5)];
    let rows = [row(10, "a/x.flac", 1, None)];
    let d = Md5Table::new(&[]).resolve(&inv, &rows);
    assert!(is_new(&d[0]));
}

#[test]
fn missing_row_is_revived_when_found_again_by_md5() {
    let inv = [entry("b/y.flac", 5)];
    let mut r = row(10, "a/x.flac", 1, Some(md5(1)));
    r.missing = true;
    let d = Md5Table::new(&[("b/y.flac", md5(1))]).resolve(&inv, &[r]);
    match &d[0].identity {
        Identity::Existing { revived, .. } => assert!(revived),
        other => panic!("{other:?}"),
    }
}

// ---------------------------------------------------------------- rel_path_key 段

#[test]
fn path_match_when_inode_and_md5_are_unavailable() {
    let mut e = entry("a/x.flac", 3); // 別 inode（tmp + rename での外部書き換え）
    e.size = 999;
    let rows = [row(10, "a/x.flac", 1, None)];
    let d = Md5Table::new(&[]).resolve(&[e], &rows);
    assert_eq!(existing(&d[0]), (10, Via::Path, true));
}

#[test]
fn in_place_rewrite_via_tmp_rename_keeps_identity_by_path_even_with_md5() {
    // 同じパスに新 inode。md5 段は「旧 key が inventory に無い」を満たさず、path 段で同一
    let e = entry("a/x.flac", 3);
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let d = Md5Table::new(&[("a/x.flac", md5(1))]).resolve(&[e], &rows);
    assert_eq!(existing(&d[0]), (10, Via::Path, true));
}

#[test]
fn path_matching_uses_canonical_key_equality() {
    // key は呼び出し側が canonical 化済み。ここでは同じ key 文字列なら一致する
    let e = entry("album/x.flac", 3);
    let rows = [row(10, "album/x.flac", 1, None)];
    let d = Md5Table::new(&[]).resolve(&[e], &rows);
    assert_eq!(existing(&d[0]).0, 10);
}

// ---------------------------------------------------------------- hardlink（受け入れ (g)）

#[test]
fn hardlinked_entry_skips_inode_and_md5_and_resolves_by_path_only() {
    let mut e = entry("b/link.flac", 1);
    e.nlink = 2;
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let t = Md5Table::new(&[("b/link.flac", md5(1))]);
    let d = t.resolve(&[e], &rows);
    assert!(is_new(&d[0]));
    assert!(d[0].hardlink);
    assert!(t.calls.borrow().is_empty(), "hardlink では md5 を読まない");

    // 同じ inode でも path が一致すれば path 段で同一
    let mut e = entry("a/x.flac", 1);
    e.nlink = 2;
    let d = t.resolve(&[e], &rows);
    assert_eq!(existing(&d[0]).1, Via::Path);
    assert!(d[0].hardlink);
}

#[test]
fn two_paths_sharing_an_inode_in_the_inventory_are_hardlinks_even_if_nlink_says_one() {
    let inv = [entry("a/x.flac", 1), entry("b/y.flac", 1)];
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let d = Md5Table::new(&[]).resolve(&inv, &rows);
    assert!(d[0].hardlink && d[1].hardlink);
    assert_eq!(existing(&d[0]).1, Via::Path);
    assert!(is_new(&d[1]));
}

// ---------------------------------------------------------------- swap / 循環（受け入れ (h)）

#[test]
fn swap_rename_follows_inodes() {
    let inv = [entry("a.flac", 2), entry("b.flac", 1)];
    let rows = [
        row(10, "a.flac", 1, Some(md5(1))),
        row(11, "b.flac", 2, Some(md5(2))),
    ];
    let d = Md5Table::new(&[]).resolve(&inv, &rows);
    assert_eq!(existing(&d[0]).0, 11);
    assert_eq!(existing(&d[1]).0, 10);
}

#[test]
fn three_way_rotation_follows_inodes() {
    // a→b, b→c, c→a
    let inv = [entry("a.flac", 3), entry("b.flac", 1), entry("c.flac", 2)];
    let rows = [
        row(10, "a.flac", 1, None),
        row(11, "b.flac", 2, None),
        row(12, "c.flac", 3, None),
    ];
    let d = Md5Table::new(&[]).resolve(&inv, &rows);
    assert_eq!(existing(&d[0]).0, 12);
    assert_eq!(existing(&d[1]).0, 10);
    assert_eq!(existing(&d[2]).0, 11);
}

// ---------------------------------------------------------------- 訪問順（受け入れ (k)）

#[test]
fn result_is_independent_of_inventory_order() {
    // コピー先を先に stat しても、コピー元が残っていれば新規のまま
    let a = entry("a/x.flac", 1);
    let copy = entry("b/copy.flac", 5);
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let t = Md5Table::new(&[("b/copy.flac", md5(1)), ("a/x.flac", md5(1))]);

    let d1 = t.resolve(&[a.clone(), copy.clone()], &rows);
    let d2 = t.resolve(&[copy, a], &rows);
    assert_eq!(existing(&d1[0]).0, 10);
    assert!(is_new(&d1[1]));
    assert!(is_new(&d2[0]));
    assert_eq!(existing(&d2[1]).0, 10);
}

#[test]
fn contention_for_one_row_is_deterministic_regardless_of_order() {
    let first = entry("b/first.flac", 5);
    let second = entry("z/second.flac", 6);
    let rows = [row(10, "a/x.flac", 1, Some(md5(1)))];
    let t = Md5Table::new(&[("z/second.flac", md5(1)), ("b/first.flac", md5(1))]);
    let d = t.resolve(&[second.clone(), first.clone()], &rows);
    let d_rev = t.resolve(&[first, second], &rows);
    assert!(is_new(&d[0]) && existing(&d[1]).0 == 10);
    assert!(existing(&d_rev[0]).0 == 10 && is_new(&d_rev[1]));
}

// ---------------------------------------------------------------- duplicate_groups（受け入れ (d)）

#[test]
fn duplicates_registered_as_new_tracks_appear_in_duplicate_groups() {
    use rusqlite::params;
    let conn = spindle::db::open_memory_connection().unwrap();
    let insert = |id: i64, path: &str, md5: &[u8], missing: Option<i64>| {
        conn.execute(
            "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, audio_md5,
                                 codec, lossless, seen_at, missing_since)
             VALUES (?1, ?2, ?2, 0, 0, 0, ?3, 'flac', 1, 0, ?4)",
            params![id, path, md5, missing],
        )
        .unwrap();
    };
    insert(1, "a/x.flac", &md5(1), None);
    insert(2, "b/copy.flac", &md5(1), None);
    insert(3, "c/other.flac", &md5(2), None);

    let groups: Vec<(Vec<u8>, i64, i64)> = conn
        .prepare("SELECT audio_md5, n, representative_id FROM duplicate_groups")
        .unwrap()
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    assert_eq!(groups, [(md5(1).to_vec(), 2, 1)]);

    // missing 行は数えない
    conn.execute("UPDATE tracks SET missing_since = 1 WHERE id = 2", [])
        .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM duplicate_groups", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
}

// ---------------------------------------------------------------- md5 の遅延（P1-0）

/// 初回スキャン（既存行なし）では md5 を 1 度も要求しない。段 2 の候補（md5 を持ち、旧 key が
/// inventory に無い未 claim の行）が存在しないときは、md5 を計算しても照合相手が無い
#[test]
fn no_md5_is_requested_when_no_row_can_be_matched_by_md5() {
    let t = Md5Table::new(&[("a", md5(1)), ("b", md5(2))]);
    let inv = vec![entry("a", 1), entry("b", 2)];
    let d = t.resolve(&inv, &[]);
    assert!(d.iter().all(is_new));
    assert!(t.calls.borrow().is_empty(), "{:?}", t.calls.borrow());

    // 既存行があっても、その旧 key が inventory に残っている（移動元が消えていない）なら候補にならない
    let rows = vec![row(1, "a", 1, Some(md5(1)))];
    let inv = vec![entry("a", 1), entry("c", 3)];
    let d = t.resolve(&inv, &rows);
    assert_eq!(existing(&d[0]), (1, Via::Inode, false));
    assert!(is_new(&d[1]));
    assert!(t.calls.borrow().is_empty(), "{:?}", t.calls.borrow());

    // 段 1 で claim された行も候補にならない（inode で見つかった行の旧 key が消えていても）
    let rows = vec![row(1, "old", 1, Some(md5(1)))];
    let inv = vec![entry("moved", 1), entry("c", 3)];
    let d = t.resolve(&inv, &rows);
    assert_eq!(
        existing(&d[0]),
        (1, Via::Inode, false),
        "物理属性は同じ（パスだけ違う）"
    );
    assert!(is_new(&d[1]));
    assert!(t.calls.borrow().is_empty(), "{:?}", t.calls.borrow());
}

/// inode 一致で size も mtime も違うときの照合は、行が md5 を持つときだけ md5 を要求する
#[test]
fn inode_reuse_check_does_not_request_md5_when_row_has_none() {
    let t = Md5Table::new(&[("a", md5(1))]);
    let mut e = entry("a", 1);
    e.size += 1;
    e.mtime_ns += 1;
    let rows = vec![row(1, "a", 1, None)];
    let d = t.resolve(&[e], &rows);
    // 段 1 は検証できず、段 3（path）で一致
    assert_eq!(existing(&d[0]), (1, Via::Path, true));
    assert!(t.calls.borrow().is_empty(), "{:?}", t.calls.borrow());
}

/// 段 2 の候補があるときだけ、未決のエントリの md5 を要求する
#[test]
fn md5_is_requested_only_when_a_move_candidate_exists() {
    let t = Md5Table::new(&[("a", md5(1)), ("new1", md5(9)), ("new2", md5(1))]);
    // 行 1 の旧 key "gone" が inventory に無い → 候補。a は inode で解決するので要求されない
    let rows = vec![
        row(1, "gone", 5, Some(md5(1))),
        row(2, "a", 1, Some(md5(7))),
    ];
    let inv = vec![entry("a", 1), entry("new1", 8), entry("new2", 9)];
    let d = t.resolve(&inv, &rows);
    assert_eq!(existing(&d[0]), (2, Via::Inode, false));
    assert!(is_new(&d[1]));
    assert_eq!(existing(&d[2]), (1, Via::AudioMd5, true));
    let mut calls = t.calls.borrow().clone();
    calls.sort();
    assert_eq!(calls, vec!["new1".to_owned(), "new2".to_owned()]);
}

/// [`md5_requests`] は `resolve` が要求しうるエントリの集合（`resolve` の要求を含む）。
/// 呼び出し側はこれを並列に計算してから `resolve` にキャッシュを渡す
#[test]
fn md5_requests_is_a_superset_of_what_resolve_asks_for() {
    use spindle::domain::identity::md5_requests;
    let t = Md5Table::new(&[("a", md5(1)), ("new1", md5(9)), ("new2", md5(1))]);
    let rows = vec![
        row(1, "gone", 5, Some(md5(1))),
        row(2, "a", 1, Some(md5(7))),
    ];
    let inv = vec![entry("a", 1), entry("new1", 8), entry("new2", 9)];
    let mut req = md5_requests(&inv, &rows);
    req.sort_unstable();
    assert_eq!(req, vec![1, 2]);
    let _ = t.resolve(&inv, &rows);
    for k in t.calls.borrow().iter() {
        let i = inv.iter().position(|e| e.key == *k).unwrap();
        assert!(req.contains(&i), "{k}");
    }
    // 候補が無ければ空
    assert!(md5_requests(&inv, &[]).is_empty());
    assert!(md5_requests(&inv, &[row(2, "a", 1, Some(md5(7)))]).is_empty());
    // 段 1 の inode 再利用の照合（size / mtime が両方違い、行が md5 を持つ）も含む
    let mut e = entry("a", 1);
    e.size += 1;
    e.mtime_ns += 1;
    assert_eq!(md5_requests(&[e], &[row(2, "a", 1, Some(md5(7)))]), vec![0]);
}
