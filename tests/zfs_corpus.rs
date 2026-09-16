//! 受け入れ (m): 対象 TrueNAS（ZFS `casesensitivity=insensitive` + `normalization=formD`）上で
//! 代表ケースの corpus を作り、`O_EXCL` の結果（FS が同名とみなすか）と canonical key の
//! 同値判定の差を記録する。差があれば D-31 の限界として文書化する。
//!
//! `SPINDLE_TEST_ZFS_DIR=/mnt/tank/scratch cargo test --test zfs_corpus -- --ignored --nocapture`

use std::fs::OpenOptions;
use std::io::ErrorKind;
use std::path::Path;

use spindle::domain::relpath::canonical_key;

/// (名前 1, 名前 2, 説明)
const CORPUS: &[(&str, &str, &str)] = &[
    ("Case-B.flac", "case-b.flac", "ASCII 大小文字"),
    ("が.flac", "か\u{3099}.flac", "濁点の NFC / NFD"),
    ("Ｂ.flac", "B.flac", "全角と半角（NFKC はしない）"),
    ("Ｂ.flac", "ｂ.flac", "全角の大小文字"),
    ("straße.flac", "STRASSE.flac", "ß の full casefold"),
    ("ẞ.flac", "ß.flac", "大文字 ẞ と ß"),
    ("I.flac", "ı.flac", "トルコ語 dotless ı"),
    ("İ.flac", "i.flac", "トルコ語 dotted İ"),
    ("ﬁ.flac", "fi.flac", "合字 ﬁ"),
    ("ǅ.flac", "ǆ.flac", "タイトルケース ǅ"),
    ("Ω.flac", "ω.flac", "ギリシャ文字"),
    (
        "Å.flac",
        "Å.flac",
        "オングストローム記号 U+212B と Å U+00C5",
    ),
    ("é.flac", "e\u{301}.flac", "é の NFC / NFD"),
];

#[test]
#[ignore = "対象 TrueNAS の ZFS 上で実行する（SPINDLE_TEST_ZFS_DIR）"]
fn record_fs_vs_key_equivalence_on_zfs() {
    let Some(dir) = std::env::var_os("SPINDLE_TEST_ZFS_DIR") else {
        panic!("SPINDLE_TEST_ZFS_DIR を設定すること");
    };
    let dir = Path::new(&dir).join(format!("spindle-corpus-{}", std::process::id()));
    std::fs::create_dir(&dir).unwrap();

    let mut differences = Vec::new();
    eprintln!("| ケース | FS 同一 | key 同一 | 一致 |");
    eprintln!("|---|---|---|---|");
    for (a, b, desc) in CORPUS {
        let create = |name: &str| {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(dir.join(name))
        };
        create(a).unwrap_or_else(|e| panic!("{a}: {e}"));
        let fs_same = match create(b) {
            Ok(_) => false,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => true,
            Err(e) => panic!("{b}: {e}"),
        };
        let key_same = canonical_key(a) == canonical_key(b);
        let agree = fs_same == key_same;
        eprintln!(
            "| {desc} (`{a}` / `{b}`) | {fs_same} | {key_same} | {} |",
            if agree { "○" } else { "**×**" }
        );
        if !agree {
            differences.push(format!("{desc}: fs={fs_same} key={key_same}"));
        }
        let _ = std::fs::remove_file(dir.join(a));
        let _ = std::fs::remove_file(dir.join(b));
    }
    let _ = std::fs::remove_dir(&dir);

    if differences.is_empty() {
        eprintln!("差なし");
    } else {
        eprintln!("差あり（D-31 の限界として docs/DECISIONS.md に記録すること）:");
        for d in &differences {
            eprintln!("  - {d}");
        }
    }
}
