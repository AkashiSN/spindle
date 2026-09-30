//! `AdbFs` を偽の adb（`adb -s <serial> shell <script>` を手元の sh で実行するスクリプト）で試す。
//! 端末の root の代わりに一時ディレクトリの絶対パスを渡す。実機の試験は `#[ignore]`

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use spindle::device::adb::{AdbConfig, AdbFs};
use spindle::device::remote::{DeviceFs, DirState, RemoteError};
use spindle::domain::device::sha256_hex;
use tokio_util::sync::CancellationToken;

fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

/// シリアルとサーバの指定を確かめてから、スクリプトを手元の sh で実行する偽の adb
fn fake_adb(_dir: &Path) -> PathBuf {
    // 書き込み中の fd を別スレッドの fork が引き継ぐと ETXTBSY になるので、最初に 1 度だけ書く
    static ONCE: std::sync::OnceLock<(tempfile::TempDir, PathBuf)> = std::sync::OnceLock::new();
    ONCE.get_or_init(write_fake_adb).1.clone()
}

fn write_fake_adb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("adb");
    std::fs::write(
        &path,
        r#"#!/bin/sh
[ "$ADB_SERVER_SOCKET" = "tcp:adb:5037" ] || { echo "cannot connect to daemon" >&2; exit 1; }
[ "$1" = "-s" ] || { echo "-s が無い" >&2; exit 2; }
[ "$2" = "SER1" ] || { echo "adb: device '$2' not found" >&2; exit 1; }
[ "$3" = "shell" ] || { echo "未対応のサブコマンド $3" >&2; exit 2; }
[ "$#" -eq 4 ] || { echo "引数の数が違う: $#" >&2; exit 2; }
exec sh -c "$4"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, path)
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    fs: AdbFs,
}

fn env_with(serial: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("storage/emulated/0/Music/spindle");
    let cfg = AdbConfig {
        program: fake_adb(dir.path()),
        server: "tcp:adb:5037".into(),
        timeout: Duration::from_secs(30),
        transfer_timeout: Duration::from_secs(60),
    };
    let fs = AdbFs::new(
        cfg,
        serial,
        root.to_str().unwrap(),
        CancellationToken::new(),
    )
    .unwrap();
    Env {
        _dir: dir,
        root,
        fs,
    }
}

fn env() -> Env {
    env_with("SER1")
}

#[test]
fn root_state_follows_the_directory() {
    block_on(async {
        let e = env();
        assert_eq!(e.fs.root_state().await.unwrap(), DirState::Missing);
        std::fs::create_dir_all(&e.root).unwrap();
        assert_eq!(e.fs.root_state().await.unwrap(), DirState::Empty);
        std::fs::write(e.root.join("x"), b"x").unwrap();
        assert_eq!(e.fs.root_state().await.unwrap(), DirState::NonEmpty);
    });
}

#[test]
fn write_read_append_with_awkward_names() {
    block_on(async {
        let e = env();
        let p = "J-Pop/it's a test/$(x) 群青.opus";
        e.fs.write(p, b"\0binary\xff").await.unwrap();
        assert_eq!(e.fs.read(p).await.unwrap(), Some(b"\0binary\xff".to_vec()));
        e.fs.append(p, b"more").await.unwrap();
        assert_eq!(
            e.fs.read(p).await.unwrap(),
            Some(b"\0binary\xffmore".to_vec())
        );
        assert_eq!(e.fs.read("nope").await.unwrap(), None);
        assert_eq!(
            e.fs.sha256(p).await.unwrap(),
            Some(sha256_hex(b"\0binary\xffmore"))
        );
        assert_eq!(e.fs.sha256("nope").await.unwrap(), None);
    });
}

#[test]
fn put_streams_the_fd_and_hashes_on_the_way() {
    block_on(async {
        let e = env();
        let body: Vec<u8> = (0..3_000_000u32).map(|i| (i % 251) as u8).collect();
        let mut src = tempfile::tempfile().unwrap();
        std::io::Write::write_all(&mut src, &body).unwrap();
        std::io::Seek::rewind(&mut src).unwrap();
        let r = e.fs.put("a/b.opus.spindle-tmp", src).await.unwrap();
        assert_eq!(r.size, body.len() as u64);
        assert_eq!(r.sha256, sha256_hex(&body));
        assert_eq!(
            std::fs::read(e.root.join("a/b.opus.spindle-tmp")).unwrap(),
            body
        );
    });
}

#[test]
fn put_fails_fast_when_the_remote_side_fails() {
    block_on(async {
        let e = env();
        std::fs::create_dir_all(&e.root).unwrap();
        std::fs::write(e.root.join("file"), b"x").unwrap();
        let body = vec![7u8; 8 << 20];
        let mut src = tempfile::tempfile().unwrap();
        std::io::Write::write_all(&mut src, &body).unwrap();
        std::io::Seek::rewind(&mut src).unwrap();
        // 親が通常ファイルなので mkdir が失敗する
        let r = tokio::time::timeout(Duration::from_secs(20), e.fs.put("file/x.opus", src)).await;
        assert!(matches!(r, Ok(Err(RemoteError::Failed(_)))), "{r:?}");
    });
}

#[test]
fn append_of_ten_thousand_lines_goes_through_stdin() {
    block_on(async {
        let e = env();
        let line = format!(
            "{{\"t\":\"batch_member\",\"pad\":\"{}\"}}\n",
            "x".repeat(300)
        );
        let bytes = line.repeat(10_000).into_bytes();
        e.fs.append(".spindle/journal", &bytes).await.unwrap();
        assert_eq!(
            e.fs.read(".spindle/journal").await.unwrap().unwrap().len(),
            bytes.len()
        );
    });
}

#[test]
fn list_files_is_unambiguous_with_newlines_in_names() {
    block_on(async {
        let e = env();
        e.fs.write("a.opus", b"12345").await.unwrap();
        e.fs.write("Dir/b c.opus", b"1").await.unwrap();
        std::fs::write(e.root.join("evil\n5 a.opus"), b"xy").unwrap();
        let mut files = e.fs.list_files().await.unwrap();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let got: Vec<(String, u64)> = files.into_iter().map(|f| (f.path, f.size)).collect();
        assert_eq!(
            got,
            vec![
                ("Dir/b c.opus".into(), 1),
                ("a.opus".into(), 5),
                ("evil\n5 a.opus".into(), 2),
            ]
        );
    });
}

#[test]
fn rename_remove_and_prune() {
    block_on(async {
        let e = env();
        e.fs.write("A/B/x.opus", b"x").await.unwrap();
        e.fs.write(".spindle/manifest.json", b"{}").await.unwrap();
        std::fs::create_dir_all(e.root.join(".spindle/moving")).unwrap();
        e.fs.rename("A/B/x.opus", "C/D/y.opus").await.unwrap();
        assert!(e.root.join("C/D/y.opus").exists());
        e.fs.prune_empty_dirs().await.unwrap();
        assert!(!e.root.join("A").exists());
        assert!(e.root.join(".spindle/moving").exists());
        e.fs.remove("C/D/y.opus").await.unwrap();
        e.fs.remove("C/D/y.opus").await.unwrap(); // 無くても成功
        e.fs.sync().await.unwrap();
        assert!(e.fs.free_bytes().await.unwrap() > 0);
    });
}

#[test]
fn free_bytes_works_before_the_root_exists() {
    block_on(async {
        let e = env();
        assert!(e.fs.free_bytes().await.unwrap() > 0);
    });
}

#[test]
fn unknown_serial_is_not_connected() {
    block_on(async {
        let e = env_with("OTHER");
        assert_eq!(e.fs.read("x").await.unwrap_err(), RemoteError::NotConnected);
    });
}

#[test]
fn paths_escaping_the_root_are_refused() {
    block_on(async {
        let e = env();
        assert!(matches!(
            e.fs.read("../x").await,
            Err(RemoteError::Failed(_))
        ));
        assert!(matches!(
            e.fs.write("/etc/x", b"").await,
            Err(RemoteError::Failed(_))
        ));
    });
}

#[test]
fn invalid_serial_is_refused_up_front() {
    let cfg = AdbConfig {
        program: "adb".into(),
        server: "tcp:adb:5037".into(),
        timeout: Duration::from_secs(1),
        transfer_timeout: Duration::from_secs(1),
    };
    assert!(AdbFs::new(
        cfg,
        "a;b",
        "/storage/emulated/0/Music/spindle",
        CancellationToken::new()
    )
    .is_err());
}

#[test]
fn engine_runs_over_the_fake_adb() {
    block_on(async {
        use spindle::device::recover::{recover, Expect};
        use spindle::device::store::initialize;
        let e = env();
        initialize(&e.fs, "u1", "emulated").await.unwrap();
        let rec = recover(
            &e.fs,
            &Expect {
                device_uuid: "u1".into(),
                volume: "emulated".into(),
            },
        )
        .await
        .unwrap();
        assert!(rec.items.is_empty());
        assert!(e.root.join(".spindle/manifest.json").exists());
    });
}

/// 実機（Xperia）での確認。`SPINDLE_TEST_ADB_SERIAL` と、adb サーバが要る。
/// `cargo test --test device_adb -- --ignored real_device` で走らせる
#[test]
#[ignore]
fn real_device_round_trip() {
    let Ok(serial) = std::env::var("SPINDLE_TEST_ADB_SERIAL") else {
        return;
    };
    block_on(async {
        let cfg = AdbConfig {
            program: std::env::var("SPINDLE_TEST_ADB")
                .unwrap_or_else(|_| "adb".into())
                .into(),
            server: std::env::var("ADB_SERVER_SOCKET")
                .unwrap_or_else(|_| "tcp:localhost:5037".into()),
            timeout: Duration::from_secs(60),
            transfer_timeout: Duration::from_secs(600),
        };
        let fs = AdbFs::new(
            cfg,
            &serial,
            "/storage/emulated/0/Music/spindle-test",
            CancellationToken::new(),
        )
        .unwrap();
        fs.write("it's/テスト.bin", b"\0\x01\xff").await.unwrap();
        assert_eq!(
            fs.read("it's/テスト.bin").await.unwrap(),
            Some(b"\0\x01\xff".to_vec())
        );
        assert_eq!(
            fs.sha256("it's/テスト.bin").await.unwrap(),
            Some(sha256_hex(b"\0\x01\xff"))
        );
        assert!(fs
            .list_files()
            .await
            .unwrap()
            .iter()
            .any(|f| f.path == "it's/テスト.bin"));
        fs.remove("it's/テスト.bin").await.unwrap();
        fs.prune_empty_dirs().await.unwrap();
        fs.rescan().await.unwrap();
    });
}

#[test]
fn prune_keeps_reserved_dir_when_root_has_glob_chars() {
    block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("Music/[spindle]");
        let cfg = AdbConfig {
            program: fake_adb(dir.path()),
            server: "tcp:adb:5037".into(),
            timeout: Duration::from_secs(30),
            transfer_timeout: Duration::from_secs(60),
        };
        let fs = AdbFs::new(
            cfg,
            "SER1",
            root.to_str().unwrap(),
            CancellationToken::new(),
        )
        .unwrap();
        std::fs::create_dir_all(root.join(".spindle/moving")).unwrap();
        std::fs::create_dir_all(root.join("empty/dir")).unwrap();
        fs.prune_empty_dirs().await.unwrap();
        assert!(root.join(".spindle/moving").exists());
        assert!(!root.join("empty").exists());
    });
}

#[test]
fn odd_roots_are_refused() {
    for root in ["/", "//", "/a//b", "/a/./b"] {
        let cfg = AdbConfig {
            program: "adb".into(),
            server: "x".into(),
            timeout: Duration::from_secs(1),
            transfer_timeout: Duration::from_secs(1),
        };
        assert!(
            AdbFs::new(cfg, "SER1", root, CancellationToken::new()).is_err(),
            "{root}"
        );
    }
}
