//! DSL の端末フィールド（仕様 ④ C）。値は端末名、演算子は IS だけ。device_pending は呼び出し側が渡す集合を引く

use std::collections::HashMap;
use std::sync::Arc;

use rusqlite::{params, Connection};
use spindle::db::devices::{self, NewDevice, Selection};
use spindle::db::open_memory_connection;
use spindle::domain::derived::Variant;
use spindle::domain::device::{PendingSets, Transport};
use spindle::playlist::{compile, dsl};

fn conn() -> Connection {
    let c = open_memory_connection().unwrap();
    for id in 1..=3 {
        c.execute(
            "INSERT INTO tracks (id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless,
                                 channels, audio_version, tag_version, seen_at)
             VALUES (?1, ?2, ?2, 1, 0, 0, 'flac', 1, 2, 1, 1, 0)",
            params![id, format!("a/{id}.flac")],
        )
        .unwrap();
    }
    let d = devices::create(
        &c,
        &NewDevice {
            name: "iPhone",
            transport: Transport::Agent,
            variant: Variant::Aac,
            selection: Selection::All,
            adb: None,
        },
        0,
    )
    .unwrap();
    c.execute(
        "INSERT INTO device_items (device_id, track_id, dest_path, dest_path_key, token, size, sha256, synced_at)
         VALUES (?1, 2, 'a/2.m4a', 'a/2.m4a', 't', 1, 's', 0)",
        [d.id],
    )
    .unwrap();
    c
}

fn sets(key: &str, ids: Vec<i64>) -> PendingSets {
    let by_key: HashMap<String, Vec<i64>> = [(key.to_owned(), ids)].into_iter().collect();
    PendingSets {
        by_key: Arc::new(by_key),
        by_id: Arc::new(HashMap::new()),
    }
}

fn eval(c: &Connection, src: &str, s: &PendingSets) -> Vec<i64> {
    compile::evaluate(c, &dsl::parse(src).unwrap(), s).unwrap()
}

#[test]
fn on_device_matches_synced_rows_by_name_key() {
    let c = conn();
    assert_eq!(
        eval(&c, "%on_device% IS iphone", &PendingSets::default()),
        vec![2]
    );
    assert_eq!(
        eval(&c, "NOT %on_device% IS iPhone", &PendingSets::default()),
        vec![1, 3]
    );
    assert!(eval(&c, "%on_device% IS 知らない端末", &PendingSets::default()).is_empty());
}

#[test]
fn device_pending_uses_given_sets() {
    let c = conn();
    let s = sets("iphone", vec![1, 3]);
    assert_eq!(eval(&c, "%device_pending% IS iPhone", &s), vec![1, 3]);
    assert!(eval(&c, "%device_pending% IS iPhone", &PendingSets::default()).is_empty());
}

#[test]
fn device_fields_accept_only_is() {
    for src in [
        "%on_device% HAS iPhone",
        "%device_pending% MATCHES i.*",
        "%on_device% GREATER 1",
        "%device_pending% PRESENT",
        "%on_device% MISSING",
        "%title% IS x ORDER BY %on_device%",
        "%title% IS x ORDER BY %device_pending% DESC",
    ] {
        let r = dsl::parse(src);
        let bad = match r {
            Ok(rule) => compile::check(&rule).is_err(),
            Err(_) => true,
        };
        assert!(bad, "{src} は拒否する");
    }
    // ORDER BY の拒否は構文ではなくコンパイラが出すこと（パース自体は通る）
    assert!(dsl::parse("%title% IS x ORDER BY %on_device%").is_ok());
}

#[test]
fn references_device_fields_finds_nested_fields() {
    let yes =
        dsl::parse("%title% IS a OR (NOT (%device_pending% IS iPhone AND %album% IS b))").unwrap();
    assert!(yes.references_device_fields());
    let order = dsl::parse("%title% IS a ORDER BY %on_device%").unwrap();
    assert!(order.references_device_fields());
    let no = dsl::parse("%title% IS on_device").unwrap();
    assert!(!no.references_device_fields());
}
