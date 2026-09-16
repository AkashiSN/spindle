//! P0-7 の受け入れ計測: 6 万件 + 履歴 10 万 op + 重複 5% の合成 DB で、100 件取得
//! （バッジ用 JOIN と total 込み）が warm cache で 100ms 以内。
//!
//! debug ビルドでは bundled SQLite も最適化なしで遅いので `#[ignore]`。計測は
//! `cargo test --release --test tracks_perf -- --ignored --nocapture` で行い、結果は
//! docs/TASKS.md に参照マシンと一緒に記す

use std::time::{Duration, Instant};

use rusqlite::{params, Connection};

use spindle::db::tracks;
use spindle::db::Db;
use spindle::domain::filter::{Cursor, Filter, Flag, Query, Sort};
use spindle::domain::selection::Selection;

const TRACKS: i64 = 60_000;
const OPS: i64 = 100_000;
const ALBUMS: i64 = 5_000;
const LIMIT_MS: u128 = 100;

fn build(conn: &Connection) {
    conn.execute_batch("BEGIN").unwrap();
    for c in ["J-Pop", "Game", "Anime", "Classical"] {
        conn.execute("INSERT INTO categories (name) VALUES (?1)", [c])
            .unwrap();
    }
    {
        let mut ins = conn
            .prepare("INSERT INTO albums (rel_dir, rel_dir_key, category_id, albumartist, album, date) VALUES (?1, ?1, ?2, ?3, ?4, ?5)")
            .unwrap();
        for a in 0..ALBUMS {
            ins.execute(params![
                format!("cat{}/artist{:04}/album{:05}", a % 4, a % 700, a),
                a % 4 + 1,
                format!("アーティスト{:04}", a % 700),
                format!("アルバム{:05}", a),
                format!("{}", 1990 + a % 35),
            ])
            .unwrap();
        }
    }
    {
        let mut ins = conn
            .prepare(
                "INSERT INTO tracks (album_id, rel_path, rel_path_key, dev, inode, nlink, size, mtime_ns, ctime_ns,
                   audio_md5, codec, lossless, duration_ms, verification, rg_scanned_at,
                   title, artist_display, album, albumartist, track_no, disc_no, date, seen_at, missing_since)
                 VALUES (?1, ?2, ?2, 1, ?3, 1, 30000000, 1, 1, ?4, 'flac', 1, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 1, ?13, 1, ?14)",
            )
            .unwrap();
        for i in 0..TRACKS {
            let album = i / 12 + 1;
            // 5% は直前の行と同じ audio_md5（重複）
            let md5_seed = if i % 20 == 19 { i - 1 } else { i };
            let mut md5 = vec![0u8; 16];
            md5[..8].copy_from_slice(&md5_seed.to_le_bytes());
            let verification = match i % 5 {
                0 => "verified_ctdb",
                1 => "verified_ar",
                2 => "unverifiable",
                _ => "not_attempted",
            };
            ins.execute(params![
                album,
                format!(
                    "cat{}/artist{:04}/album{:05}/{:02} 曲{:05}.flac",
                    album % 4,
                    album % 700,
                    album,
                    i % 12 + 1,
                    i
                ),
                i + 1000,
                md5,
                180_000 + (i * 37) % 240_000,
                verification,
                if i % 3 == 0 { None } else { Some(1i64) },
                format!("曲{:05} タイトル", (i * 7919) % TRACKS),
                format!("アーティスト{:04}", album % 700),
                format!("アルバム{:05}", album),
                format!("アーティスト{:04}", album % 700),
                i % 12 + 1,
                format!("{}", 1990 + album % 35),
                if i % 200 == 0 {
                    Some(1i64)
                } else {
                    None::<i64>
                },
            ])
            .unwrap();
        }
    }
    {
        // 履歴 10 万 op: 1000 バッチ × 100 op。最後の 100 バッチの一部を pending / conflict に
        let mut batch = conn
            .prepare("INSERT INTO edit_batches (created_at, description, state) VALUES (?1, 'bulk', 'applied')")
            .unwrap();
        let mut op = conn
            .prepare("INSERT INTO edit_ops (batch_id, ordinal, track_id, kind, result) VALUES (?1, ?2, ?3, 'tags', ?4)")
            .unwrap();
        for b in 0..(OPS / 100) {
            batch.execute([b + 1]).unwrap();
            for o in 0..100 {
                let track = (b * 100 + o) % TRACKS + 1;
                let result = if b == OPS / 100 - 1 && o % 2 == 0 {
                    "pending"
                } else if o % 25 == 0 {
                    "skipped_conflict"
                } else {
                    "applied"
                };
                op.execute(params![b + 1, o, track, result]).unwrap();
            }
        }
    }
    conn.execute_batch("COMMIT").unwrap();
}

fn query(filter: Filter, sort: &str) -> Query {
    Query {
        filter,
        sort: Sort::parse(sort).unwrap(),
        cursor: None,
        limit: 100,
    }
}

/// warm cache で 5 回測り、中央値を返す
fn measure(conn: &Connection, q: &Query) -> (Duration, i64) {
    let _ = tracks::list(conn, q).unwrap();
    let mut times = Vec::new();
    let mut total = 0;
    for _ in 0..5 {
        let t0 = Instant::now();
        let page = tracks::list(conn, q).unwrap();
        times.push(t0.elapsed());
        assert!(!page.items.is_empty() && page.items.len() <= 100);
        total = page.total;
    }
    times.sort();
    (times[2], total)
}

#[test]
#[ignore = "release ビルドで手動計測する（cargo test --release --test tracks_perf -- --ignored --nocapture）"]
fn list_100_rows_with_badges_and_total_under_100ms_on_synthetic_60k() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("perf.db");
    let db = Db::open(&path).unwrap();
    drop(db);
    let conn = Connection::open(&path).unwrap();
    let t0 = Instant::now();
    build(&conn);
    eprintln!("合成 DB 構築: {:?}", t0.elapsed());
    let n: i64 = conn
        .query_row("SELECT count(*) FROM duplicate_groups", [], |r| r.get(0))
        .unwrap();
    eprintln!("duplicate_groups: {n} 組");

    let cases: Vec<(&str, Query)> = vec![
        ("既定（album 順）1 ページ目", query(Filter::default(), "")),
        ("title 昇順", query(Filter::default(), "title")),
        ("duration 降順", query(Filter::default(), "-duration")),
        (
            "category=J-Pop + album 順",
            query(
                Filter {
                    category: Some("J-Pop".into()),
                    ..Default::default()
                },
                "",
            ),
        ),
        (
            "albumartist 1 人（ツリー葉）",
            query(
                Filter {
                    albumartist: Some("アーティスト0007".into()),
                    ..Default::default()
                },
                "",
            ),
        ),
        (
            "flags=[duplicate]",
            query(
                Filter {
                    flags: vec![Flag::Duplicate],
                    ..Default::default()
                },
                "",
            ),
        ),
        (
            "flags=[pending]",
            query(
                Filter {
                    flags: vec![Flag::Pending],
                    ..Default::default()
                },
                "title",
            ),
        ),
        (
            "flags=[conflict]",
            query(
                Filter {
                    flags: vec![Flag::Conflict],
                    ..Default::default()
                },
                "",
            ),
        ),
        (
            "flags=[no_rg] + -date",
            query(
                Filter {
                    flags: vec![Flag::NoRg],
                    ..Default::default()
                },
                "-date",
            ),
        ),
        (
            "検索 FTS「タイトル」",
            query(
                Filter {
                    q: Some("タイトル".into()),
                    ..Default::default()
                },
                "",
            ),
        ),
        (
            "検索 LIKE「曲1」",
            query(
                Filter {
                    q: Some("曲1".into()),
                    ..Default::default()
                },
                "",
            ),
        ),
    ];
    let mut worst = Duration::ZERO;
    for (name, q) in &cases {
        let (t, total) = measure(&conn, q);
        let plan = tracks::explain_list(&conn, q).unwrap();
        let temp = plan.iter().any(|l| l.contains("TEMP B-TREE"));
        eprintln!(
            "{name:40} {:>7.2} ms  total={total:<6} temp_btree={temp}",
            t.as_secs_f64() * 1000.0
        );
        worst = worst.max(t);
    }
    // 中間ページ（カーソルあり）
    let mut q = query(Filter::default(), "");
    let first = tracks::list(&conn, &q).unwrap();
    q.cursor = Some(Cursor::decode(first.next_cursor.as_deref().unwrap()).unwrap());
    let (t, _) = measure(&conn, &q);
    eprintln!(
        "{:40} {:>7.2} ms",
        "既定 2 ページ目",
        t.as_secs_f64() * 1000.0
    );
    worst = worst.max(t);
    // 深いページ（最終ページ付近まで飛ぶ）
    let mut q = query(Filter::default(), "title");
    let mut hops = 0;
    loop {
        let page = tracks::list(&conn, &q).unwrap();
        hops += 1;
        match page.next_cursor {
            Some(c) if hops < 599 => q.cursor = Some(Cursor::decode(&c).unwrap()),
            _ => break,
        }
    }
    let (t, _) = measure(&conn, &q);
    eprintln!(
        "{:40} {:>7.2} ms",
        "title 599 ページ目",
        t.as_secs_f64() * 1000.0
    );
    worst = worst.max(t);

    // フィルタ形 selection の解決（6 万件、ID 列挙なし）
    let t0 = Instant::now();
    let rows = tracks::resolve_selection(
        &conn,
        &Selection::Filter {
            filter: Filter::default(),
            exclude_ids: vec![5, 9],
        },
    )
    .unwrap();
    let t = t0.elapsed();
    eprintln!(
        "{:40} {:>7.2} ms  rows={}",
        "selection filter 形（全件）",
        t.as_secs_f64() * 1000.0,
        rows.len()
    );
    assert_eq!(rows.len() as i64, TRACKS - 2);

    eprintln!(
        "最悪値: {:.2} ms（上限 {LIMIT_MS} ms）",
        worst.as_secs_f64() * 1000.0
    );
    assert!(worst.as_millis() <= LIMIT_MS, "100 件取得が {worst:?}");
}
