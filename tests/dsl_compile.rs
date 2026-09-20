//! AST → パラメータ化 SQL と `:memory:` での評価（docs/DSL.md「SQL 生成」、D-54）。
//! 列名はホワイトリスト、値はすべてバインド、任意タグは track_tags の EXISTS、
//! `missing` を参照しない限り active だけ

use rusqlite::{params, Connection};

use spindle::db::open_memory_connection;
use spindle::playlist::compile::{check, evaluate, CompileError};
use spindle::playlist::dsl::parse;

fn conn() -> Connection {
    open_memory_connection().unwrap()
}

struct T<'a> {
    id: i64,
    title: &'a str,
    artist: &'a str,
    album: &'a str,
    albumartist: &'a str,
    date: &'a str,
    codec: &'a str,
    rate: i64,
    duration_ms: i64,
    track_no: i64,
    missing: bool,
    added_at: i64,
    album_id: Option<i64>,
    tags: &'a [(&'a str, &'a str)],
}

fn insert(c: &Connection, t: &T) {
    let lossless = i64::from(matches!(t.codec, "flac" | "alac" | "wav"));
    c.execute(
        "INSERT INTO tracks (id, album_id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
           sample_rate, bit_depth, channels, bitrate, duration_ms, title, artist_display, album, albumartist,
           track_no, disc_no, date, seen_at, missing_since, added_at, verification)
         VALUES (?1, ?2, 'x/' || ?1 || '.flac', 'x/' || ?1 || '.flac', 1, 0, 0, ?3, ?4, ?5, 16, 2, 900, ?6,
                 ?7, ?8, ?9, ?10, ?11, 1, ?12, 1, ?13, ?14, 'not_attempted')",
        params![
            t.id,
            t.album_id,
            t.codec,
            lossless,
            t.rate,
            t.duration_ms,
            t.title,
            t.artist,
            t.album,
            t.albumartist,
            t.track_no,
            t.date,
            if t.missing { Some(1) } else { None },
            t.added_at,
        ],
    )
    .unwrap();
    for (i, (k, v)) in t.tags.iter().enumerate() {
        c.execute(
            "INSERT INTO track_tags (track_id, key, idx, value) VALUES (?1, ?2, ?3, ?4)",
            params![t.id, k, i as i64, v],
        )
        .unwrap();
    }
}

/// 3 曲 + missing 1 曲。id=1 は J-Pop の album、Derived あり
fn fixture() -> Connection {
    let c = conn();
    c.execute("INSERT INTO categories (id, name) VALUES (1, 'J-Pop')", [])
        .unwrap();
    c.execute(
        "INSERT INTO albums (id, rel_dir, rel_dir_key, category_id, album) VALUES (1, 'J-Pop/A/X', 'j-pop/a/x', 1, 'X')",
        [],
    )
    .unwrap();
    let base = T {
        id: 0,
        title: "",
        artist: "",
        album: "",
        albumartist: "",
        date: "",
        codec: "flac",
        rate: 44100,
        duration_ms: 200_000,
        track_no: 1,
        missing: false,
        added_at: 1_700_000_000,
        album_id: None,
        tags: &[],
    };
    insert(
        &c,
        &T {
            id: 1,
            title: "Crow Song",
            artist: "Girls Dead Monster",
            album: "X",
            albumartist: "ヰ世界情緒",
            date: "2024-03-01",
            album_id: Some(1),
            tags: &[
                ("GENRE", "Anime"),
                ("GENRE", "Rock"),
                ("COMPOSER", "Jun Maeda"),
            ],
            ..base
        },
    );
    insert(
        &c,
        &T {
            id: 2,
            title: "alchemy",
            artist: "GDM",
            albumartist: "ヰ世界情緒",
            date: "2020",
            codec: "opus",
            rate: 48000,
            duration_ms: 90_000,
            track_no: 2,
            added_at: 1_600_000_000,
            tags: &[("GENRE", "anime")],
            ..base
        },
    );
    insert(
        &c,
        &T {
            id: 3,
            title: "My Song",
            artist: "LiSA",
            albumartist: "花譜",
            date: "2024-05-05",
            rate: 96000,
            duration_ms: 400_000,
            track_no: 3,
            tags: &[("COMMENT", "hi-res")],
            ..base
        },
    );
    insert(
        &c,
        &T {
            id: 4,
            title: "Gone",
            artist: "X",
            albumartist: "ヰ世界情緒",
            missing: true,
            track_no: 4,
            ..base
        },
    );
    c.execute(
        "INSERT INTO derived_files (track_id, rel_path, rel_path_key, codec, src_audio_version, src_tag_version, generated_at)
         VALUES (1, 'x/1.opus', 'x/1.opus', 'opus', 1, 1, 0)",
        [],
    )
    .unwrap();
    // 偽ハイレゾ検出（P3-5）: 2 は upsampled、3 は ok（崖なし・実効 16 bit）、1 と 4 は未検査
    c.execute_batch(
        "UPDATE tracks SET hires_check = 'upsampled', hires_checked_at = 1, hires_check_version = 1,
                           hires_cutoff_hz = 22050, hires_cliff_db = 48.5, hires_effective_bits = 24 WHERE id = 2;
         UPDATE tracks SET hires_check = 'ok', hires_checked_at = 1, hires_check_version = 1,
                           hires_cutoff_hz = 48000, hires_cliff_db = NULL, hires_effective_bits = 16 WHERE id = 3;",
    )
    .unwrap();
    c
}

fn eval(c: &Connection, src: &str) -> Vec<i64> {
    evaluate(c, &parse(src).unwrap()).unwrap()
}

#[test]
fn is_on_cache_columns_is_case_insensitive_and_excludes_missing_by_default() {
    let c = fixture();
    assert_eq!(eval(&c, "%albumartist% IS ヰ世界情緒"), vec![1, 2]);
    assert_eq!(eval(&c, "%title% IS \"crow song\""), vec![1]);
    assert_eq!(eval(&c, "%artist% IS lisa"), vec![3]);
    assert_eq!(eval(&c, "%album artist% IS 花譜"), vec![3]);
}

#[test]
fn missing_field_switches_off_the_implicit_active_filter() {
    let c = fixture();
    assert_eq!(eval(&c, "%missing% IS true"), vec![4]);
    assert_eq!(eval(&c, "%missing% IS false"), vec![1, 2, 3]);
    assert_eq!(
        eval(&c, "%albumartist% IS ヰ世界情緒 AND NOT %missing% IS true"),
        vec![1, 2]
    );
    assert_eq!(
        eval(&c, "%albumartist% IS ヰ世界情緒 OR %missing% IS true"),
        vec![1, 2, 4]
    );
}

#[test]
fn has_and_matches_and_presence() {
    let c = fixture();
    assert_eq!(eval(&c, "%title% HAS song"), vec![1, 3]);
    assert_eq!(eval(&c, "%title% HAS \"%\""), Vec::<i64>::new()); // LIKE のメタ文字はエスケープ
    assert_eq!(eval(&c, "%title% MATCHES \"^(?i)my\""), vec![3]);
    assert_eq!(eval(&c, "%title% MATCHES \"Song$\""), vec![1, 3]);
    assert_eq!(eval(&c, "PRESENT %date%"), vec![1, 2, 3]);
    assert_eq!(eval(&c, "MISSING %album%"), vec![2, 3]);
}

#[test]
fn arbitrary_tags_expand_to_track_tags_exists_and_match_any_value() {
    let c = fixture();
    assert_eq!(eval(&c, "%genre% IS anime"), vec![1, 2]);
    assert_eq!(eval(&c, "%genre% IS rock"), vec![1]);
    assert_eq!(eval(&c, "%genre% HAS ock"), vec![1]);
    assert_eq!(eval(&c, "%composer% MATCHES \"^Jun\""), vec![1]);
    assert_eq!(eval(&c, "PRESENT %comment%"), vec![3]);
    assert_eq!(eval(&c, "MISSING %genre%"), vec![3]);
    assert_eq!(eval(&c, "NOT %genre% IS anime"), vec![3]);
}

#[test]
fn numeric_fields_compare_as_numbers_and_duration_is_seconds() {
    let c = fixture();
    assert_eq!(eval(&c, "%samplerate% GREATER 44100"), vec![2, 3]);
    assert_eq!(eval(&c, "%samplerate% IS 96000"), vec![3]);
    assert_eq!(eval(&c, "%duration% LESS 100"), vec![2]);
    assert_eq!(eval(&c, "%duration% GREATER 199.5"), vec![1, 3]);
    assert_eq!(
        eval(&c, "%tracknumber% GREATER 1 AND %tracknumber% LESS 3"),
        vec![2]
    );
    assert_eq!(eval(&c, "%bitdepth% IS 16"), vec![1, 2, 3]);
    assert_eq!(
        eval(&c, "%channels% IS 2 AND %bitrate% IS 900"),
        vec![1, 2, 3]
    );
}

#[test]
fn extension_fields() {
    let c = fixture();
    assert_eq!(eval(&c, "%category% IS j-pop"), vec![1]);
    assert_eq!(eval(&c, "MISSING %category%"), vec![2, 3]);
    assert_eq!(eval(&c, "%lossless% IS true"), vec![1, 3]);
    assert_eq!(eval(&c, "%lossless% IS no"), vec![2]);
    assert_eq!(eval(&c, "%codec% IS opus"), vec![2]);
    assert_eq!(eval(&c, "%has_derived% IS true"), vec![1]);
    assert_eq!(eval(&c, "%verification% IS not_attempted"), vec![1, 2, 3]);
    assert_eq!(eval(&c, "%source_type% IS unknown"), vec![1, 2, 3]);
}

#[test]
fn date_and_added_compare_lexically_and_by_day() {
    let c = fixture();
    assert_eq!(eval(&c, "%date% GREATER 2024"), vec![1, 3]);
    assert_eq!(eval(&c, "%date% LESS 2024-04"), vec![1, 2]);
    assert_eq!(eval(&c, "%date% HAS 2024"), vec![1, 3]);
    // added は日付（YYYY-MM-DD、UTC）か epoch 秒
    assert_eq!(eval(&c, "%added% GREATER 2023-01-01"), vec![1, 3]);
    assert_eq!(eval(&c, "%added% LESS 1650000000"), vec![2]);
    assert_eq!(eval(&c, "%added% IS 2023-11-14"), vec![1, 3]);
    // 境界付近でも溢れなければ通る（-2^63 + 86400 は日の頭に丸めて +86400 が収まる）
    assert!(check(&parse("%added% IS -9223372036854689408").unwrap()).is_ok());
    // GREATER / LESS の整数は足し引きしないので両端でも通る（空集合になるだけ）
    assert!(check(&parse("%added% GREATER 9223372036854775807").unwrap()).is_ok());
    assert!(check(&parse("%added% LESS -9223372036854775808").unwrap()).is_ok());
    // 閏日は妥当
    assert!(check(&parse("%added% IS 2024-02-29").unwrap()).is_ok());
    assert!(check(&parse("%added% IS 2023-02-29").unwrap()).is_err());
}

#[test]
fn order_and_limit_and_random() {
    let c = fixture();
    assert_eq!(
        eval(
            &c,
            "%lossless% IS true OR %lossless% IS false ORDER BY %title% DESC"
        ),
        vec![3, 1, 2]
    );
    assert_eq!(
        eval(&c, "PRESENT %title% ORDER BY %duration% ASC LIMIT 2"),
        vec![2, 1]
    );
    assert_eq!(eval(&c, "PRESENT %title% ORDER BY %genre%"), vec![1, 2, 3]); // Anime, anime, NULL → NULL は末尾
    let mut r = eval(&c, "PRESENT %title% ORDER BY random");
    r.sort();
    assert_eq!(r, vec![1, 2, 3]);
    // 既定は id 順
    assert_eq!(eval(&c, "PRESENT %title%"), vec![1, 2, 3]);
}

#[test]
fn semantic_errors_are_reported_before_running() {
    for (src, expect) in [
        ("%title% GREATER 1", "greater"),
        ("%genre% LESS 1", "less"),
        ("%samplerate% IS abc", "数値"),
        ("%lossless% IS maybe", "真偽"),
        ("%lossless% HAS t", "真偽"),
        ("%title% MATCHES \"(\"", "正規表現"),
        ("%added% IS yesterday", "日付"),
        ("%added% IS 2023-02-31", "日付"),
        ("%added% IS 2023-13-01", "日付"),
        ("%added% GREATER 99999999999-01-01", "日付"),
        ("%added% LESS 0000-00-00", "日付"),
        // epoch 秒の両端: 日の切り出しと +86400 が溢れる値は拒否（panic しない）
        ("%added% IS -9223372036854775808", "epoch"),
        ("%added% IS 9223372036854775807", "epoch"),
        ("%missing% GREATER 1", "真偽"),
    ] {
        let err = check(&parse(src).unwrap()).unwrap_err();
        let msg = err.to_string().to_lowercase();
        assert!(msg.contains(&expect.to_lowercase()), "{src}: {err}");
        assert!(matches!(
            err,
            CompileError::Field { .. } | CompileError::Value { .. }
        ));
    }
    // ORDER BY の未知フィールドはタグとして通る
    assert!(check(&parse("PRESENT %title% ORDER BY %whatever%").unwrap()).is_ok());
}

#[test]
fn values_are_bound_not_interpolated() {
    let c = fixture();
    // 値に SQL の断片が入っても壊れない・効かない
    assert_eq!(eval(&c, "%title% IS \"x' OR 1=1 --\""), Vec::<i64>::new());
    assert_eq!(
        eval(&c, "%genre% IS \"anime' OR '1'='1\""),
        Vec::<i64>::new()
    );
    assert_eq!(eval(&c, "%title% HAS \"' OR ''='\""), Vec::<i64>::new());
}

#[test]
fn hires_check_fields_compare_status_and_measurements() {
    let c = fixture();
    assert_eq!(eval(&c, "%hirescheck% IS upsampled"), vec![2]);
    assert_eq!(eval(&c, "%hirescheck% IS ok"), vec![3]);
    assert_eq!(eval(&c, "MISSING %hirescheck%"), vec![1]);
    assert_eq!(eval(&c, "%cutoff% LESS 25000"), vec![2]);
    assert_eq!(eval(&c, "%cutoff% IS 48000"), vec![3]);
    assert_eq!(eval(&c, "%cliff% GREATER 30"), vec![2]);
    assert_eq!(
        eval(&c, "%cliff% GREATER 30.5 AND %cliff% LESS 50"),
        vec![2]
    );
    assert_eq!(eval(&c, "PRESENT %cliff%"), vec![2]);
    assert_eq!(eval(&c, "%effectivebits% IS 16"), vec![3]);
    assert_eq!(eval(&c, "%effectivebits% LESS 24"), vec![3]);
}
