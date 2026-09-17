//! トラック一覧クエリ（P0-7）: キーセットページング、ソート、フィルタ、検索の FTS / LIKE
//! フォールバック、バッジ列、selection の解決、EXPLAIN QUERY PLAN。
//! 仕様: docs/SPEC.md §9、docs/DECISIONS.md D-33 / D-39

use std::collections::HashSet;

use rusqlite::{params, Connection};

use spindle::db::open_memory_connection;
use spindle::db::tracks::{self, TrackRow};
use spindle::domain::filter::{Cursor, Filter, Flag, KeyType, Query, Sort};
use spindle::domain::selection::Selection;

/// 挿入する行の指定。`None` は NULL
#[derive(Default, Clone)]
struct T {
    title: Option<&'static str>,
    artist: Option<&'static str>,
    album: Option<&'static str>,
    albumartist: Option<&'static str>,
    album_id: Option<i64>,
    disc: Option<i64>,
    track: Option<i64>,
    date: Option<&'static str>,
    codec: &'static str,
    duration: Option<i64>,
    md5: Option<u8>,
    nlink: i64,
    missing: Option<i64>,
    rg_scanned: Option<i64>,
    verification: &'static str,
}

fn t() -> T {
    T {
        codec: "flac",
        nlink: 1,
        verification: "not_attempted",
        ..Default::default()
    }
}

fn insert(conn: &Connection, rel: &str, s: &T) -> i64 {
    let key = spindle::domain::relpath::canonical_key(rel);
    let lossless = matches!(s.codec, "flac" | "alac" | "wav");
    let md5 = s.md5.map(|b| vec![b; 16]);
    conn.execute(
        "INSERT INTO tracks (album_id, rel_path, rel_path_key, dev, inode, nlink, size, mtime_ns, ctime_ns,
           audio_md5, codec, lossless, duration_ms, verification, rg_scanned_at,
           title, artist_display, album, albumartist, track_no, disc_no, date, seen_at, missing_since)
         VALUES (?1, ?2, ?3, 1, abs(random()), ?4, 100, 1, 1, ?5, ?6, ?7, ?8, ?9, ?10,
           ?11, ?12, ?13, ?14, ?15, ?16, ?17, 1, ?18)",
        params![
            s.album_id,
            rel,
            key,
            s.nlink,
            md5,
            s.codec,
            lossless as i64,
            s.duration,
            s.verification,
            s.rg_scanned,
            s.title,
            s.artist,
            s.album,
            s.albumartist,
            s.track,
            s.disc,
            s.date,
            s.missing,
        ],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn insert_album(
    conn: &Connection,
    rel_dir: &str,
    category: Option<i64>,
    aa: &str,
    album: &str,
) -> i64 {
    conn.execute(
        "INSERT INTO albums (rel_dir, rel_dir_key, category_id, albumartist, album) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            rel_dir,
            spindle::domain::relpath::canonical_key(rel_dir),
            category,
            aa,
            album
        ],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn insert_category(conn: &Connection, name: &str) -> i64 {
    conn.execute("INSERT INTO categories (name) VALUES (?1)", [name])
        .unwrap();
    conn.last_insert_rowid()
}

fn insert_batch(conn: &Connection) -> i64 {
    conn.execute(
        "INSERT INTO edit_batches (created_at, description) VALUES (1, 'x')",
        [],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn insert_op(conn: &Connection, batch: i64, track: i64, result: &str) -> i64 {
    let ordinal: i64 = conn
        .query_row(
            "SELECT coalesce(max(ordinal), 0) + 1 FROM edit_ops WHERE batch_id = ?1",
            [batch],
            |r| r.get(0),
        )
        .unwrap();
    conn.execute(
        "INSERT INTO edit_ops (batch_id, ordinal, track_id, kind, result) VALUES (?1, ?2, ?3, 'tags', ?4)",
        params![batch, ordinal, track, result],
    )
    .unwrap();
    conn.last_insert_rowid()
}

fn ids(rows: &[TrackRow]) -> Vec<i64> {
    rows.iter().map(|r| r.id).collect()
}

fn query(filter: Filter, sort: &str, limit: usize) -> Query {
    Query {
        filter,
        sort: Sort::parse(sort).unwrap(),
        cursor: None,
        limit,
    }
}

/// カーソルを辿って全ページを集める。各ページで total が変わらないことも確認する
fn walk(conn: &Connection, mut q: Query) -> (Vec<TrackRow>, usize) {
    let mut all = Vec::new();
    let mut pages = 0;
    let mut total = None;
    loop {
        let page = tracks::list(conn, &q).unwrap();
        pages += 1;
        assert!(page.items.len() <= q.limit);
        match total {
            None => total = Some(page.total),
            Some(t) => assert_eq!(page.total, t),
        }
        all.extend(page.items);
        match page.next_cursor {
            Some(c) => q.cursor = Some(Cursor::decode(&c).unwrap()),
            None => break,
        }
    }
    assert_eq!(
        all.len() as i64,
        total.unwrap(),
        "total は全ページの合計と一致"
    );
    (all, pages)
}

// ---------------------------------------------------------------- ソートとページング

#[test]
fn default_sort_is_album_order_and_cursor_pages_without_gaps_or_dups() {
    let conn = open_memory_connection().unwrap();
    let a1 = insert_album(&conn, "A/X", None, "A", "X");
    let a2 = insert_album(&conn, "A/Y", None, "A", "Y");
    let b1 = insert_album(&conn, "B/Z", None, "B", "Z");
    // 順は (albumartist, album_id, disc, track)。NULL は 0 / '' 扱いで先頭
    let e = insert(
        &conn,
        "B/Z/2.flac",
        &T {
            albumartist: Some("B"),
            album_id: Some(b1),
            disc: Some(1),
            track: Some(2),
            ..t()
        },
    );
    let d = insert(
        &conn,
        "B/Z/1.flac",
        &T {
            albumartist: Some("B"),
            album_id: Some(b1),
            disc: Some(1),
            track: Some(1),
            ..t()
        },
    );
    let c = insert(
        &conn,
        "A/Y/1.flac",
        &T {
            albumartist: Some("A"),
            album_id: Some(a2),
            disc: Some(1),
            track: Some(1),
            ..t()
        },
    );
    let b = insert(
        &conn,
        "A/X/2.flac",
        &T {
            albumartist: Some("A"),
            album_id: Some(a1),
            disc: Some(1),
            track: Some(2),
            ..t()
        },
    );
    let a = insert(
        &conn,
        "A/X/1.flac",
        &T {
            albumartist: Some("A"),
            album_id: Some(a1),
            disc: Some(1),
            track: Some(1),
            ..t()
        },
    );
    let n = insert(&conn, "loose.flac", &t());
    let n2 = insert(
        &conn,
        "loose2.flac",
        &T {
            albumartist: Some("A"),
            ..t()
        },
    );

    let (all, pages) = walk(&conn, query(Filter::default(), "", 2));
    assert_eq!(pages, 4, "7 件を 2 件ずつ");
    assert_eq!(ids(&all), vec![n, n2, a, b, c, d, e]);

    let (desc, _) = walk(&conn, query(Filter::default(), "-album", 3));
    assert_eq!(ids(&desc), vec![e, d, c, b, a, n2, n]);
}

#[test]
fn sort_by_each_key_in_both_directions_is_total_and_stable() {
    let conn = open_memory_connection().unwrap();
    let mut expect_ids = HashSet::new();
    for i in 0..23 {
        let title: Option<&'static str> = match i % 4 {
            0 => None,
            1 => Some("あ"),
            2 => Some("い"),
            _ => Some("a"),
        };
        let dur = if i % 5 == 0 {
            None
        } else {
            Some((i % 7) as i64 * 1000)
        };
        let codec = if i % 3 == 0 { "opus" } else { "flac" };
        let date = if i % 6 == 0 { None } else { Some("2020") };
        let rel = format!("d/{i:02}.flac");
        let rel: &'static str = Box::leak(rel.into_boxed_str());
        let id = insert(
            &conn,
            rel,
            &T {
                title,
                duration: dur,
                codec,
                date,
                artist: title,
                albumartist: title,
                album: title,
                ..t()
            },
        );
        expect_ids.insert(id);
    }
    for key in [
        "album",
        "title",
        "artist",
        "album_title",
        "albumartist",
        "date",
        "duration",
        "codec",
        "rel_path",
        "id",
    ] {
        for dir in ["", "-"] {
            let s = format!("{dir}{key}");
            let (all, _) = walk(&conn, query(Filter::default(), &s, 4));
            let got: HashSet<i64> = ids(&all).into_iter().collect();
            assert_eq!(got.len(), all.len(), "{s}: 重複なし");
            assert_eq!(got, expect_ids, "{s}: 欠落なし");
            // 1 ページ目と全体の先頭が一致（カーソルが順序を変えない）
            let first = tracks::list(&conn, &query(Filter::default(), &s, 100)).unwrap();
            assert_eq!(
                ids(&first.items),
                ids(&all),
                "{s}: 1 ページで取っても同じ順"
            );
        }
    }
}

#[test]
fn null_sort_keys_come_first_ascending_and_last_descending() {
    let conn = open_memory_connection().unwrap();
    let null = insert(&conn, "n.flac", &t());
    let a = insert(
        &conn,
        "a.flac",
        &T {
            title: Some("a"),
            ..t()
        },
    );
    let b = insert(
        &conn,
        "b.flac",
        &T {
            title: Some("b"),
            ..t()
        },
    );
    let (asc, _) = walk(&conn, query(Filter::default(), "title", 1));
    assert_eq!(ids(&asc), vec![null, a, b]);
    let (desc, _) = walk(&conn, query(Filter::default(), "-title", 1));
    assert_eq!(ids(&desc), vec![b, a, null]);
}

#[test]
fn mismatched_cursor_yields_empty_page_not_error() {
    let conn = open_memory_connection().unwrap();
    insert(
        &conn,
        "a.flac",
        &T {
            title: Some("a"),
            ..t()
        },
    );
    insert(
        &conn,
        "b.flac",
        &T {
            title: Some("b"),
            duration: Some(5),
            ..t()
        },
    );
    // album ソートのカーソル（4 キー）を title ソートに渡す
    let mut q = query(Filter::default(), "title", 10);
    q.cursor = Some(Cursor {
        sort: Sort::parse("album").unwrap(),
        keys: vec!["".to_owned().into(), 0.into(), 0.into(), 0.into()],
        id: 0,
    });
    let page = tracks::list(&conn, &q).unwrap();
    assert!(page.items.is_empty());
    assert_eq!(page.next_cursor, None);
    assert_eq!(page.total, 2, "total はフィルタだけで数える");

    // 同じキー数でも型が違う（duration の整数カーソルを title に流用）。INTEGER を TEXT 式と
    // 比べると SQLite の storage class 順序で常に真になり先頭ページが再掲されるので、空にする
    for (dir, from) in [("", "duration"), ("-", "-duration")] {
        let mut dq = query(Filter::default(), from, 1);
        let first = tracks::list(&conn, &dq).unwrap();
        let c = Cursor::decode(first.next_cursor.as_deref().unwrap()).unwrap();
        assert_eq!(
            c.keys,
            vec![spindle::domain::filter::CursorValue::Int(
                first.items[0].duration_ms.unwrap_or(-1)
            )]
        );
        // 発行時ソートを title に差し替えて渡す（形は 1 キー + id で同じ）
        let forged = Cursor {
            sort: Sort::parse(&format!("{dir}title")).unwrap(),
            keys: c.keys.clone(),
            id: c.id,
        };
        let mut tq = query(Filter::default(), &format!("{dir}title"), 10);
        tq.cursor = Some(forged);
        let page = tracks::list(&conn, &tq).unwrap();
        assert!(
            page.items.is_empty(),
            "{dir}title に整数カーソル: {:?}",
            ids(&page.items)
        );
        // 正規のカーソルは向きが違うだけでも使わない
        dq.cursor = Some(Cursor {
            sort: Sort::parse(if dir.is_empty() {
                "-duration"
            } else {
                "duration"
            })
            .unwrap(),
            ..c.clone()
        });
        assert!(tracks::list(&conn, &dq).unwrap().items.is_empty());
    }
}

// ---------------------------------------------------------------- フィルタ

#[test]
fn filters_by_tree_playlist_and_flags() {
    let conn = open_memory_connection().unwrap();
    let jpop = insert_category(&conn, "J-Pop");
    let game = insert_category(&conn, "Game");
    let a1 = insert_album(&conn, "J-Pop/A/X", Some(jpop), "A", "X");
    let a2 = insert_album(&conn, "Game/B/Y", Some(game), "B", "Y");
    let x1 = insert(
        &conn,
        "J-Pop/A/X/1.flac",
        &T {
            albumartist: Some("A"),
            album_id: Some(a1),
            md5: Some(1),
            ..t()
        },
    );
    let x2 = insert(
        &conn,
        "J-Pop/A/X/2.flac",
        &T {
            albumartist: Some("A"),
            album_id: Some(a1),
            md5: Some(1),
            verification: "verified_ctdb",
            rg_scanned: Some(5),
            ..t()
        },
    );
    let y1 = insert(
        &conn,
        "Game/B/Y/1.flac",
        &T {
            albumartist: Some("B"),
            album_id: Some(a2),
            nlink: 2,
            ..t()
        },
    );
    let gone = insert(
        &conn,
        "gone.flac",
        &T {
            md5: Some(1),
            missing: Some(10),
            rg_scanned: Some(1),
            ..t()
        },
    );
    conn.execute(
        "INSERT INTO playlists (name, name_key, created_at, updated_at) VALUES ('p', 'p', 1, 1)",
        [],
    )
    .unwrap();
    conn.execute("INSERT INTO playlist_items (playlist_id, position, track_id) VALUES (1, 0, ?1), (1, 1, ?2)", params![y1, gone]).unwrap();
    let b1 = insert_batch(&conn);
    insert_op(&conn, b1, x1, "pending");
    let b2 = insert_batch(&conn);
    insert_op(&conn, b2, y1, "skipped_conflict");
    // x2 は古い conflict の後に applied があるので conflict ではない
    insert_op(&conn, b2, x2, "skipped_conflict");
    let b3 = insert_batch(&conn);
    insert_op(&conn, b3, x2, "applied");

    let run = |f: Filter| {
        let page = tracks::list(&conn, &query(f, "id", 100)).unwrap();
        assert_eq!(page.total, page.items.len() as i64);
        ids(&page.items)
    };
    assert_eq!(
        run(Filter {
            category: Some("J-Pop".into()),
            ..Default::default()
        }),
        vec![x1, x2]
    );
    assert_eq!(
        run(Filter {
            category: Some("Nope".into()),
            ..Default::default()
        }),
        Vec::<i64>::new()
    );
    assert_eq!(
        run(Filter {
            albumartist: Some("B".into()),
            ..Default::default()
        }),
        vec![y1]
    );
    assert_eq!(
        run(Filter {
            album_id: Some(a1),
            ..Default::default()
        }),
        vec![x1, x2]
    );
    assert_eq!(
        run(Filter {
            playlist_id: Some(1),
            ..Default::default()
        }),
        vec![y1, gone]
    );
    let flag = |fl: Flag| {
        run(Filter {
            flags: vec![fl],
            ..Default::default()
        })
    };
    assert_eq!(flag(Flag::Unverified), vec![x1, y1, gone]);
    assert_eq!(
        flag(Flag::Duplicate),
        vec![x1, x2],
        "missing 行は重複に数えない"
    );
    assert_eq!(flag(Flag::Missing), vec![gone]);
    assert_eq!(flag(Flag::NoRg), vec![x1, y1]);
    assert_eq!(flag(Flag::Pending), vec![x1]);
    assert_eq!(
        flag(Flag::Conflict),
        vec![y1],
        "最新 op が skipped_conflict のものだけ"
    );
    assert_eq!(flag(Flag::Hardlink), vec![y1]);
    // 複数 flag は AND
    assert_eq!(
        run(Filter {
            flags: vec![Flag::Unverified, Flag::Duplicate],
            ..Default::default()
        }),
        vec![x1]
    );
    // ツリー + flag
    assert_eq!(
        run(Filter {
            category: Some("J-Pop".into()),
            flags: vec![Flag::Pending],
            ..Default::default()
        }),
        vec![x1]
    );
}

#[test]
fn badges_are_returned_per_row() {
    let conn = open_memory_connection().unwrap();
    let jpop = insert_category(&conn, "J-Pop");
    let a1 = insert_album(&conn, "J-Pop/A/X", Some(jpop), "A", "X");
    let x1 = insert(
        &conn,
        "J-Pop/A/X/1.flac",
        &T {
            title: Some("one"),
            albumartist: Some("A"),
            album_id: Some(a1),
            md5: Some(0xab),
            ..t()
        },
    );
    let x2 = insert(
        &conn,
        "J-Pop/A/X/2.flac",
        &T {
            albumartist: Some("A"),
            album_id: Some(a1),
            md5: Some(0xab),
            nlink: 3,
            ..t()
        },
    );
    let x3 = insert(
        &conn,
        "J-Pop/A/X/3.flac",
        &T {
            albumartist: Some("A"),
            album_id: Some(a1),
            ..t()
        },
    );
    let b1 = insert_batch(&conn);
    insert_op(&conn, b1, x1, "pending");
    let b2 = insert_batch(&conn);
    insert_op(&conn, b2, x2, "skipped_conflict");
    conn.execute(
        "INSERT INTO derived_files (track_id, rel_path, rel_path_key, codec, src_audio_version, src_tag_version, generated_at)
         VALUES (?1, 'd/1.opus', 'd/1.opus', 'opus', 1, 1, 1), (?2, 'd/3.opus', 'd/3.opus', 'opus', 1, 1, 1)",
        params![x1, x3],
    )
    .unwrap();
    conn.execute("UPDATE tracks SET tag_version = 2 WHERE id = ?1", [x3])
        .unwrap();

    let page = tracks::list(&conn, &query(Filter::default(), "id", 100)).unwrap();
    let [r1, r2, r3] = page.items.as_slice() else {
        panic!("{:?}", page.items)
    };
    assert_eq!(r1.category.as_deref(), Some("J-Pop"));
    assert_eq!(r1.title.as_deref(), Some("one"));
    assert_eq!(r1.pending_batch_id, Some(b1));
    assert_eq!(r1.conflict_batch_id, None);
    assert_eq!(
        r1.duplicate_group.as_deref(),
        Some("abababababababababababababababab")
    );
    assert_eq!(
        r1.derived,
        Some(tracks::Derived {
            codec: "opus".into(),
            stale_tags: false
        })
    );
    assert!(!r1.hardlink);
    assert!(r1.lossless);

    assert_eq!(r2.pending_batch_id, None);
    assert_eq!(r2.conflict_batch_id, Some(b2));
    assert_eq!(r2.duplicate_group, r1.duplicate_group);
    assert!(r2.hardlink);
    assert_eq!(r2.derived, None);

    assert_eq!(r3.duplicate_group, None);
    assert_eq!(
        r3.derived,
        Some(tracks::Derived {
            codec: "opus".into(),
            stale_tags: true
        })
    );

    let one = tracks::get(&conn, x2).unwrap().unwrap();
    assert_eq!(&one, r2);
    assert!(tracks::get(&conn, 9999).unwrap().is_none());
}

// ---------------------------------------------------------------- 検索

#[test]
fn search_uses_fts_for_three_or_more_chars_and_like_below() {
    let conn = open_memory_connection().unwrap();
    let a = insert(
        &conn,
        "1.flac",
        &T {
            title: Some("ヰ世界情緒 の 歌"),
            artist: Some("Artist One"),
            ..t()
        },
    );
    let b = insert(
        &conn,
        "2.flac",
        &T {
            title: Some("別の曲"),
            album: Some("情緒アルバム"),
            ..t()
        },
    );
    let c = insert(
        &conn,
        "3.flac",
        &T {
            title: Some("50% off_"),
            ..t()
        },
    );
    let _d = insert(
        &conn,
        "4.flac",
        &T {
            title: Some("nothing"),
            ..t()
        },
    );

    let run = |q: &str| {
        let f = Filter {
            q: Some(q.to_owned()),
            ..Default::default()
        };
        let page = tracks::list(&conn, &query(f, "id", 100)).unwrap();
        assert_eq!(page.total, page.items.len() as i64, "{q}: total も同じ条件");
        ids(&page.items)
    };
    // 3 文字以上: trigram（日本語の部分一致）
    assert_eq!(run("世界情緒"), vec![a]);
    assert_eq!(run("情緒ア"), vec![b]);
    assert_eq!(run("ist On"), vec![a], "artist_display も索引対象");
    // 2 文字: LIKE フォールバック。title と album の両方から引ける
    assert_eq!(run("情緒"), vec![a, b]);
    assert_eq!(run("off"), vec![c]);
    // LIKE のメタ文字はリテラル
    assert_eq!(run("%"), vec![c]);
    assert_eq!(run("_"), vec![c]);
    assert_eq!(run("50% off_"), vec![c], "FTS 側でも記号を含めて一致");
    // FTS 構文文字を含めても構文エラーにならない
    assert_eq!(run("a\"b OR c"), Vec::<i64>::new());
    assert_eq!(run("xyz NOT"), Vec::<i64>::new());
}

// ---------------------------------------------------------------- selection

#[test]
fn selection_resolves_both_forms_to_the_same_rows() {
    let conn = open_memory_connection().unwrap();
    let a = insert(
        &conn,
        "a.flac",
        &T {
            title: Some("x"),
            ..t()
        },
    );
    let b = insert(
        &conn,
        "b.flac",
        &T {
            title: Some("x"),
            missing: Some(1),
            ..t()
        },
    );
    let c = insert(
        &conn,
        "c.flac",
        &T {
            title: Some("y"),
            ..t()
        },
    );
    conn.execute("UPDATE tracks SET tag_version = 7 WHERE id = ?1", [c])
        .unwrap();

    let by_ids = tracks::resolve_selection(&conn, &Selection::Ids(vec![c, a, a, 9999])).unwrap();
    assert_eq!(
        by_ids.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![a, c],
        "id 昇順・重複と不明 id は落ちる"
    );
    assert_eq!(by_ids[1].tag_version, 7);
    assert_eq!(by_ids[0].rel_path, "a.flac");

    let by_filter = tracks::resolve_selection(
        &conn,
        &Selection::Filter {
            filter: Filter::default(),
            exclude_ids: vec![b],
        },
    )
    .unwrap();
    assert_eq!(by_filter, by_ids);

    let missing_only = tracks::resolve_selection(
        &conn,
        &Selection::Filter {
            filter: Filter {
                flags: vec![Flag::Missing],
                ..Default::default()
            },
            exclude_ids: vec![],
        },
    )
    .unwrap();
    assert_eq!(
        missing_only.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![b]
    );
}

// ---------------------------------------------------------------- 実行計画

fn assert_no_temp_btree(plan: &[String], what: &str) {
    for line in plan {
        assert!(
            !line.contains("TEMP B-TREE") && !line.contains("AUTOMATIC"),
            "{what}: {line}\n{}",
            plan.join("\n")
        );
    }
}

#[test]
fn list_and_count_plans_have_no_temp_btree_for_indexed_sorts() {
    let conn = open_memory_connection().unwrap();
    // 本番は ANALYZE を走らせない（sqlite_stat1 なし）ので、統計なしの計画を見る
    for i in 0..50 {
        let rel: &'static str = Box::leak(format!("x/{i}.flac").into_boxed_str());
        let title: &'static str = Box::leak(format!("t{i}").into_boxed_str());
        insert(
            &conn,
            rel,
            &T {
                title: Some(title),
                albumartist: Some("a"),
                duration: Some(i * 10),
                md5: Some((i % 10) as u8),
                ..t()
            },
        );
    }
    for key in [
        "album",
        "title",
        "artist",
        "album_title",
        "albumartist",
        "date",
        "duration",
        "codec",
        "rel_path",
        "id",
    ] {
        for dir in ["", "-"] {
            let s = format!("{dir}{key}");
            let mut q = query(Filter::default(), &s, 100);
            let plan = tracks::explain_list(&conn, &q).unwrap();
            assert_no_temp_btree(&plan, &format!("sort={s} 1 ページ目"));
            // 2 ページ目（カーソルあり）も同じ索引を範囲検索で使う
            let page = tracks::list(&conn, &q).unwrap();
            q.cursor = page
                .next_cursor
                .map(|c| Cursor::decode(&c).unwrap())
                .or_else(|| {
                    Some(Cursor {
                        sort: Sort::parse(&s).unwrap(),
                        keys: Sort::parse(&s)
                            .unwrap()
                            .key
                            .key_types()
                            .iter()
                            .map(|t| match t {
                                KeyType::Int => 0.into(),
                                KeyType::Text => String::new().into(),
                            })
                            .collect(),
                        id: 1,
                    })
                });
            let plan = tracks::explain_list(&conn, &q).unwrap();
            assert_no_temp_btree(&plan, &format!("sort={s} 2 ページ目"));
        }
    }
    let plan = tracks::explain_count(&conn, &Filter::default()).unwrap();
    assert_no_temp_btree(&plan, "count");
    let plan = tracks::explain_count(
        &conn,
        &Filter {
            flags: vec![Flag::Duplicate, Flag::Pending, Flag::Conflict],
            ..Default::default()
        },
    )
    .unwrap();
    assert_no_temp_btree(&plan, "count with badge flags");
}

// ---------------------------------------------------------------- プレイリスト順（P1-6、D-53）

fn insert_playlist(conn: &Connection, name: &str, track_ids: &[i64]) -> i64 {
    conn.execute(
        "INSERT INTO playlists (name, name_key, created_at, updated_at) VALUES (?1, lower(?1), 0, 0)",
        [name],
    )
    .unwrap();
    let id = conn.last_insert_rowid();
    for (pos, t) in track_ids.iter().enumerate() {
        conn.execute(
            "INSERT INTO playlist_items (playlist_id, position, track_id) VALUES (?1, ?2, ?3)",
            params![id, pos as i64, t],
        )
        .unwrap();
    }
    id
}

#[test]
fn position_sort_follows_playlist_order_and_pages_by_cursor() {
    let conn = open_memory_connection().unwrap();
    let mut all = Vec::new();
    for i in 0..7 {
        let rel: &'static str = Box::leak(format!("x/{i}.flac").into_boxed_str());
        let title: &'static str = Box::leak(format!("t{i}").into_boxed_str());
        all.push(insert(
            &conn,
            rel,
            &T {
                title: Some(title),
                albumartist: Some("a"),
                ..t()
            },
        ));
    }
    // 逆順 + 1 本抜き
    let order: Vec<i64> = all.iter().rev().skip(1).copied().collect();
    let pl = insert_playlist(&conn, "p", &order);
    let filter = Filter {
        playlist_id: Some(pl),
        ..Default::default()
    };
    let (rows, pages) = walk(&conn, query(filter.clone(), "position", 2));
    assert_eq!(ids(&rows), order);
    assert_eq!(pages, 3);
    let (rows, _) = walk(&conn, query(filter.clone(), "-position", 4));
    let mut rev = order.clone();
    rev.reverse();
    assert_eq!(ids(&rows), rev);
    // 別のプレイリストに絞れば別の並び
    let pl2 = insert_playlist(&conn, "q", &all[..3]);
    let (rows, _) = walk(
        &conn,
        query(
            Filter {
                playlist_id: Some(pl2),
                ..Default::default()
            },
            "position",
            10,
        ),
    );
    assert_eq!(ids(&rows), all[..3].to_vec());
    // 他のフィルタと組み合わせても順は保たれる（q で 1 本に絞る）
    let (rows, _) = walk(
        &conn,
        query(
            Filter {
                playlist_id: Some(pl),
                q: Some("t5".to_owned()),
                ..Default::default()
            },
            "position",
            10,
        ),
    );
    assert_eq!(ids(&rows), vec![all[5]]);
}

#[test]
fn position_sort_requires_playlist_filter() {
    // from_params は 400 相当のエラー。Sort::parse 自体は通る（cursor の復号で使う）
    assert!(Sort::parse("position").is_ok());
    let err = Query::from_params(None, Some("position"), None, None).unwrap_err();
    assert!(
        matches!(err, spindle::domain::filter::FilterError::Sort(_)),
        "{err}"
    );
    let err =
        Query::from_params(Some(r#"{"album_id":1}"#), Some("-position"), None, None).unwrap_err();
    assert!(
        matches!(err, spindle::domain::filter::FilterError::Sort(_)),
        "{err}"
    );
    assert!(Query::from_params(Some(r#"{"playlist_id":1}"#), Some("position"), None, None).is_ok());
}

#[test]
fn position_sort_plans_have_no_temp_btree() {
    let conn = open_memory_connection().unwrap();
    let mut all = Vec::new();
    for i in 0..50 {
        let rel: &'static str = Box::leak(format!("x/{i}.flac").into_boxed_str());
        all.push(insert(
            &conn,
            rel,
            &T {
                albumartist: Some("a"),
                ..t()
            },
        ));
    }
    let pl = insert_playlist(&conn, "p", &all);
    let filter = Filter {
        playlist_id: Some(pl),
        ..Default::default()
    };
    for s in ["position", "-position"] {
        let mut q = query(filter.clone(), s, 10);
        let plan = tracks::explain_list(&conn, &q).unwrap();
        assert_no_temp_btree(&plan, &format!("sort={s} 1 ページ目"));
        let page = tracks::list(&conn, &q).unwrap();
        q.cursor = Some(Cursor::decode(&page.next_cursor.unwrap()).unwrap());
        let plan = tracks::explain_list(&conn, &q).unwrap();
        assert_no_temp_btree(&plan, &format!("sort={s} 2 ページ目"));
    }
    let plan = tracks::explain_count(&conn, &filter).unwrap();
    assert_no_temp_btree(&plan, "count by playlist");
}

#[test]
fn selection_resolves_in_position_order_when_asked() {
    let conn = open_memory_connection().unwrap();
    let mut all = Vec::new();
    for i in 0..4 {
        let rel: &'static str = Box::leak(format!("x/{i}.flac").into_boxed_str());
        all.push(insert(
            &conn,
            rel,
            &T {
                albumartist: Some("a"),
                ..t()
            },
        ));
    }
    let order = vec![all[2], all[0], all[3]];
    let pl = insert_playlist(&conn, "p", &order);
    let sel = Selection::Filter {
        filter: Filter {
            playlist_id: Some(pl),
            ..Default::default()
        },
        exclude_ids: vec![],
    };
    let rows =
        tracks::resolve_selection_sorted(&conn, &sel, Some(Sort::parse("position").unwrap()))
            .unwrap();
    let got: Vec<i64> = rows.iter().map(|r| r.id).collect();
    assert_eq!(got, order);
    // ids 形 + position: どのプレイリストの順か決まらないので id 順に倒す
    let sel = Selection::Ids(vec![all[1], all[3], all[2]]);
    let rows =
        tracks::resolve_selection_sorted(&conn, &sel, Some(Sort::parse("position").unwrap()))
            .unwrap();
    let got: Vec<i64> = rows.iter().map(|r| r.id).collect();
    assert_eq!(got, vec![all[1], all[2], all[3]]);
}
