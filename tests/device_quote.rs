//! 端末側シェルへのクォートと入力の検査（仕様 ⑤「接続の構成」）

use std::process::Command;

use spindle::device::quote::*;

/// クォートした文字列を本物の sh に渡し、元の文字列がそのまま 1 引数として届くこと
fn round_trip(s: &str) -> String {
    let q = sh_quote(s).unwrap();
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("printf %s {q}"))
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn quote_survives_the_shell() {
    for s in [
        "plain",
        "with space",
        "it's",
        "''",
        "$(rm -rf /)",
        "`x`",
        "a\"b",
        "J-Pop/YOASOBI/THE BOOK/1-01 群青.opus",
        "-leading-dash",
        "semi;colon&amp|pipe",
        "back\\slash",
        "*glob?",
    ] {
        assert_eq!(round_trip(s), s, "{s}");
    }
}

#[test]
fn quote_rejects_nul_and_newlines() {
    assert_eq!(sh_quote("a\0b"), Err(QuoteError::Nul));
    assert_eq!(sh_quote("a\nb"), Err(QuoteError::Newline));
    assert_eq!(sh_quote("a\rb"), Err(QuoteError::Newline));
}

#[test]
fn serial_allowlist() {
    assert!(valid_serial("QV7123ABCD"));
    assert!(valid_serial("192.168.0.2:5555"));
    assert!(valid_serial("emulator-5554"));
    assert!(!valid_serial(""));
    assert!(!valid_serial("a b"));
    assert!(!valid_serial("a;b"));
    assert!(!valid_serial(&"x".repeat(129)));
}

#[test]
fn volume_allowlist() {
    assert!(valid_volume("emulated"));
    assert!(valid_volume("1A2B-3C4D"));
    assert!(valid_volume("1a2b-3c4d"));
    assert!(!valid_volume("1A2B3C4D"));
    assert!(!valid_volume("../x"));
    assert!(!valid_volume("1A2B-3C4G"));
}

#[test]
fn root_must_be_relative_without_dotdot() {
    assert!(valid_root("Music/spindle"));
    assert!(!valid_root("/Music"));
    assert!(!valid_root("Music/../x"));
    assert!(!valid_root(""));
}

#[test]
fn root_abs_joins_volume_and_root() {
    assert_eq!(
        root_abs("emulated", "Music/spindle").as_deref(),
        Some("/storage/emulated/0/Music/spindle")
    );
    assert_eq!(
        root_abs("1A2B-3C4D", "Music/spindle").as_deref(),
        Some("/storage/1A2B-3C4D/Music/spindle")
    );
    assert_eq!(root_abs("bad", "Music"), None);
    assert_eq!(root_abs("emulated", "../x"), None);
}
