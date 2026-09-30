//! `AdbFs` を偽の adb（`adb -s <serial> shell <script>` を手元の sh で実行するスクリプト）で試す。
//! 端末の root の代わりに一時ディレクトリの絶対パスを渡す。実機の試験は `#[ignore]`

mod device_support;

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

/// 偽の adb 一式。書き込み中の fd を別スレッドの fork が引き継ぐと ETXTBSY になるので、
/// どの試験も最初の実行より前に、全部を 1 度だけ書く
struct Fakes {
    _dir: tempfile::TempDir,
    adb: PathBuf,
    broken_stat: PathBuf,
    /// stat が壊れていて、さらに find が子の非 0 を終了コードに伝えない端末
    silent_find: PathBuf,
}

fn fakes() -> &'static Fakes {
    static ONCE: std::sync::OnceLock<Fakes> = std::sync::OnceLock::new();
    ONCE.get_or_init(write_fakes)
}

/// シリアルとサーバの指定を確かめてから、スクリプトを手元の sh で実行する偽の adb
fn fake_adb(_dir: &Path) -> PathBuf {
    fakes().adb.clone()
}

/// 終了コードに頼らず、出力の印だけで一覧の失敗に気づくかを試す偽の adb
fn fake_adb_with_silent_find() -> PathBuf {
    fakes().silent_find.clone()
}

/// `stat -c '%s %n'`（一覧のサイズ取り）だけが必ず失敗する端末の偽の adb。
/// 手置きのファイルが一覧から消えると、上書きしてよいファイルと区別できなくなる
fn fake_adb_with_broken_stat() -> PathBuf {
    fakes().broken_stat.clone()
}

fn write_executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn write_fakes() -> Fakes {
    let (dir, adb) = write_fake_adb();
    let which = |name: &str| {
        std::env::var("PATH")
            .unwrap()
            .split(':')
            .map(|d| Path::new(d).join(name))
            .find(|p| p.is_file())
            .unwrap_or_else(|| panic!("{name} が見つからない"))
    };
    let real = which("stat");
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    write_executable(
        &bin.join("stat"),
        &format!(
            "#!/bin/sh\nif [ \"$1\" = \"-c\" ] && [ \"$2\" = \"%s %n\" ]; then echo \"stat: 読めない\" >&2; exit 1; fi\nexec {} \"$@\"\n",
            real.display()
        ),
    );
    let broken_stat = dir.path().join("adb-broken-stat");
    write_executable(
        &broken_stat,
        &format!(
            "#!/bin/sh\nPATH={}:$PATH\nexport PATH\nexec {} \"$@\"\n",
            bin.display(),
            adb.display()
        ),
    );
    let quiet = dir.path().join("quiet");
    std::fs::create_dir(&quiet).unwrap();
    std::fs::copy(bin.join("stat"), quiet.join("stat")).unwrap();
    write_executable(
        &quiet.join("find"),
        &format!("#!/bin/sh\n{} \"$@\"\nexit 0\n", which("find").display()),
    );
    let silent_find = dir.path().join("adb-silent-find");
    write_executable(
        &silent_find,
        &format!(
            "#!/bin/sh\nPATH={}:$PATH\nexport PATH\nexec {} \"$@\"\n",
            quiet.display(),
            adb.display()
        ),
    );
    Fakes {
        _dir: dir,
        adb,
        broken_stat,
        silent_find,
    }
}

fn write_fake_adb() -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("adb");
    std::fs::write(
        &path,
        r#"#!/bin/sh
[ "$ADB_SERVER_SOCKET" = "tcp:adb:5037" ] || { echo "* cannot connect to daemon at $ADB_SERVER_SOCKET" >&2; exit 1; }
[ -n "$HOME" ] && [ -d "$HOME" ] && [ -w "$HOME" ] || { echo "adb_utils.cpp:315 Cannot mkdir '$HOME/.android': Permission denied" >&2; exit 134; }
if [ "$1" = version ]; then printf 'Android Debug Bridge version 1.0.41\nVersion 37.0.1-15733141\nInstalled as /x\n'; exit 0; fi
[ "$1" = "-s" ] || { echo "-s が無い" >&2; exit 2; }
[ "$2" = "SER1" ] || { echo "adb: device '$2' not found" >&2; exit 1; }
if [ "$3" = "get-state" ]; then
  [ "$#" -eq 3 ] || { echo "引数の数が違う: $#" >&2; exit 2; }
  if [ -f "$HOME/offline" ]; then echo "adb: device '$2' not found" >&2; exit 1; fi
  if [ -f "$HOME/unauthorized" ]; then echo "unauthorized"; exit 0; fi
  echo device; exit 0
fi
[ "$3" = "shell" ] || { echo "未対応のサブコマンド $3" >&2; exit 2; }
[ "$#" -eq 4 ] || { echo "引数の数が違う: $#" >&2; exit 2; }
# 転送中に抜かれた端末: stderr を出さずに 255（spike 1）
if [ -f "$HOME/drop" ]; then exit 255; fi
exec sh -c "$4"
"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    (dir, path)
}

fn home_in(dir: &Path) -> PathBuf {
    let h = dir.join("home");
    std::fs::create_dir_all(&h).unwrap();
    h
}

struct Env {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    cfg: AdbConfig,
    fs: AdbFs,
}

fn env_with(serial: &str) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("storage/emulated/0/Music/spindle");
    let home = home_in(dir.path());
    let cfg = AdbConfig {
        program: fake_adb(dir.path()),
        server: "tcp:adb:5037".into(),
        home: home.clone(),
        timeout: Duration::from_secs(30),
        transfer_timeout: Duration::from_secs(60),
    };
    let fs = AdbFs::new(
        cfg.clone(),
        serial,
        root.to_str().unwrap(),
        CancellationToken::new(),
    )
    .unwrap();
    Env {
        _dir: dir,
        root,
        home,
        cfg,
        fs,
    }
}

fn env() -> Env {
    env_with("SER1")
}

/// 同じ root を、stat の壊れた端末として見る `AdbFs`
fn broken_stat_fs(root: &Path, home: &Path) -> AdbFs {
    fs_with(fake_adb_with_broken_stat(), root, home)
}

fn fs_with(program: PathBuf, root: &Path, home: &Path) -> AdbFs {
    let cfg = AdbConfig {
        program,
        server: "tcp:adb:5037".into(),
        home: home.to_owned(),
        timeout: Duration::from_secs(30),
        transfer_timeout: Duration::from_secs(60),
    };
    AdbFs::new(
        cfg,
        "SER1",
        root.to_str().unwrap(),
        CancellationToken::new(),
    )
    .unwrap()
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
fn list_files_reads_awkward_names() {
    block_on(async {
        let e = env();
        e.fs.write("a.opus", b"12345").await.unwrap();
        e.fs.write("Dir/b c.opus", b"1").await.unwrap();
        e.fs.write("アーティスト/曲 名 (feat. x).opus", b"xy")
            .await
            .unwrap();
        let mut files = e.fs.list_files().await.unwrap();
        files.sort_by(|a, b| a.path.cmp(&b.path));
        let got: Vec<(String, u64)> = files.into_iter().map(|f| (f.path, f.size)).collect();
        assert_eq!(
            got,
            vec![
                ("Dir/b c.opus".into(), 1),
                ("a.opus".into(), 5),
                ("アーティスト/曲 名 (feat. x).opus".into(), 2),
            ]
        );
    });
}

#[test]
fn list_files_fails_on_a_newline_in_a_name() {
    block_on(async {
        // Android の FUSE では作れない名前だが、万一あっても一覧を黙って欠かさず失敗させる
        let e = env();
        e.fs.write("a.opus", b"12345").await.unwrap();
        std::fs::write(e.root.join("evil\n5 a.opus"), b"xy").unwrap();
        assert!(matches!(
            e.fs.list_files().await,
            Err(RemoteError::Failed(_))
        ));
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
        home: std::env::temp_dir(),
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
        let home = std::env::temp_dir().join("spindle-adb-home");
        std::fs::create_dir_all(&home).unwrap();
        let cfg = AdbConfig {
            program: std::env::var("SPINDLE_TEST_ADB")
                .unwrap_or_else(|_| "adb".into())
                .into(),
            server: std::env::var("ADB_SERVER_SOCKET")
                .unwrap_or_else(|_| "tcp:localhost:5037".into()),
            home,
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
            home: home_in(dir.path()),
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
            home: std::env::temp_dir(),
            timeout: Duration::from_secs(1),
            transfer_timeout: Duration::from_secs(1),
        };
        assert!(
            AdbFs::new(cfg, "SER1", root, CancellationToken::new()).is_err(),
            "{root}"
        );
    }
}

#[test]
fn list_files_fails_when_stat_fails() {
    block_on(async {
        let e = env();
        e.fs.write("hand.opus", b"hand").await.unwrap();
        let fs = broken_stat_fs(&e.root, &e.home);
        assert!(
            matches!(fs.list_files().await, Err(RemoteError::Failed(_))),
            "1 件でもサイズを取れなければ一覧全体を失敗させる"
        );
        // stat の他の使い方（空き容量）は壊していない
        assert!(fs.free_bytes().await.unwrap() > 0);
    });
}

#[test]
fn list_files_fails_even_if_find_hides_the_failure() {
    block_on(async {
        let e = env();
        e.fs.write("hand.opus", b"hand").await.unwrap();
        e.fs.write("Dir/b.opus", b"b").await.unwrap();
        let fs = fs_with(fake_adb_with_silent_find(), &e.root, &e.home);
        assert!(
            matches!(fs.list_files().await, Err(RemoteError::Failed(_))),
            "find の終了コードが 0 でも、件数の不一致で一覧全体を失敗させる"
        );
    });
}

#[test]
fn list_files_of_a_missing_root_is_empty() {
    block_on(async {
        let e = env();
        assert!(e.fs.list_files().await.unwrap().is_empty());
    });
}

#[test]
fn sync_stops_before_writing_when_listing_fails() {
    block_on(async {
        use device_support::{expect, MemSources, TestControl};
        use spindle::device::journal::random_id;
        use spindle::device::plan::{PlanItem, Runnable};
        use spindle::device::recover::recover;
        use spindle::device::store::initialize;
        use spindle::device::sync::{self, SyncInput};
        use spindle::domain::device::OpKind;
        let e = env();
        initialize(&e.fs, "u1", "emulated").await.unwrap();
        let rec = recover(&e.fs, &expect()).await.unwrap();
        // 手置きのファイル（manifest に無い）と同じパスへ曲を足す計画
        std::fs::write(e.root.join("a.opus"), b"hand").unwrap();
        let journal = std::fs::read(e.root.join(".spindle/journal")).ok();
        let body = b"spindle".to_vec();
        let runnable = Runnable {
            items: vec![PlanItem {
                op_id: random_id().unwrap(),
                op: OpKind::Add,
                track_id: 1,
                from: None,
                to: Some("a.opus".into()),
                token: Some("t1".into()),
                size: body.len() as u64,
                sha256: Some(sha256_hex(&body)),
            }],
            ..Default::default()
        };
        let fs = broken_stat_fs(&e.root, &e.home);
        let bodies = std::collections::HashMap::new();
        let r = sync::run(
            &fs,
            &MemSources([(1, body)].into_iter().collect()),
            &TestControl::default(),
            SyncInput {
                generation: 1,
                start: rec.manifest,
                runnable: &runnable,
                playlist_bodies: &bodies,
            },
        )
        .await;
        assert!(r.is_err(), "{r:?}");
        assert_eq!(std::fs::read(e.root.join("a.opus")).unwrap(), b"hand");
        assert_eq!(std::fs::read(e.root.join(".spindle/journal")).ok(), journal);
        // recover も一覧の失敗で止まる（手置きを残す）
        assert!(recover(&fs, &expect()).await.is_err());
        assert_eq!(std::fs::read(e.root.join("a.opus")).unwrap(), b"hand");
    });
}

#[test]
fn adb_gets_a_writable_home() {
    block_on(async {
        let e = env();
        // HOME を渡していなければ偽の adb は 134 で落ちる
        e.fs.write("a.bin", b"x").await.unwrap();
        assert_eq!(e.fs.read("a.bin").await.unwrap(), Some(b"x".to_vec()));
    });
}

#[test]
fn silent_255_with_the_device_gone_is_not_connected() {
    block_on(async {
        let e = env();
        std::fs::write(e.home.join("drop"), b"").unwrap();
        std::fs::write(e.home.join("offline"), b"").unwrap();
        assert_eq!(e.fs.sync().await, Err(RemoteError::NotConnected));
        assert_eq!(
            e.fs.put("x.bin", tempfile::tempfile().unwrap())
                .await
                .map(|_| ()),
            Err(RemoteError::NotConnected)
        );
    });
}

#[test]
fn silent_255_while_the_device_is_still_there_is_a_failure() {
    block_on(async {
        let e = env();
        std::fs::write(e.home.join("drop"), b"").unwrap();
        match e.fs.sync().await {
            Err(RemoteError::Failed(m)) => assert!(m.contains("255"), "{m}"),
            other => panic!("{other:?}"),
        }
    });
}

#[test]
fn unauthorized_state_counts_as_not_connected() {
    block_on(async {
        let e = env();
        std::fs::write(e.home.join("drop"), b"").unwrap();
        std::fs::write(e.home.join("unauthorized"), b"").unwrap();
        assert_eq!(e.fs.sync().await, Err(RemoteError::NotConnected));
    });
}

#[test]
fn get_state_reports_device() {
    block_on(async {
        let e = env();
        assert_eq!(e.fs.get_state().await.unwrap(), "device");
    });
}

#[test]
fn discard_init_removes_only_spindle_dir_and_empty_root() {
    block_on(async {
        let e = env();
        spindle::device::store::initialize(&e.fs, "u1", "emulated")
            .await
            .unwrap();
        assert!(e.root.join(".spindle/manifest.json").is_file());
        e.fs.discard_init().await.unwrap();
        assert!(!e.root.exists(), "空になった root も消える");

        // 手置きのファイルがあれば root は残す
        std::fs::create_dir_all(e.root.join(".spindle")).unwrap();
        std::fs::write(e.root.join("keep.txt"), b"k").unwrap();
        e.fs.discard_init().await.unwrap();
        assert!(e.root.join("keep.txt").is_file());
        assert!(!e.root.join(".spindle").exists());
    });
}

#[test]
fn probe_volumes_lists_internal_and_card_with_state() {
    block_on(async {
        let dir = tempfile::tempdir().unwrap();
        let storage = dir.path().join("storage");
        std::fs::create_dir_all(storage.join("emulated/0/Music/spindle")).unwrap();
        std::fs::create_dir_all(storage.join("E1C6-6113/Music/spindle/.spindle")).unwrap();
        std::fs::write(
            storage.join("E1C6-6113/Music/spindle/.spindle/manifest.json"),
            b"{}",
        )
        .unwrap();
        std::fs::create_dir_all(storage.join("self")).unwrap();
        let cfg = AdbConfig {
            program: fake_adb(dir.path()),
            server: "tcp:adb:5037".into(),
            home: home_in(dir.path()),
            timeout: Duration::from_secs(30),
            transfer_timeout: Duration::from_secs(60),
        };
        let v = spindle::device::adb::probe_volumes_under(
            &cfg,
            "SER1",
            storage.to_str().unwrap(),
            "Music/spindle",
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        let got: Vec<(&str, DirState)> = v.iter().map(|x| (x.volume.as_str(), x.state)).collect();
        assert_eq!(
            got,
            vec![
                ("emulated", DirState::Empty),
                ("E1C6-6113", DirState::NonEmpty)
            ]
        );
        assert!(v.iter().all(|x| x.free > 0));
        assert_eq!(
            v[0].path,
            format!("{}/emulated/0/Music/spindle", storage.display())
        );
    });
}

/// 実機で、登録 → 追加 → 大小文字だけの改名 → 削除 を回す。
/// `SPINDLE_TEST_ADB_SERIAL` / `ADB_SERVER_SOCKET`（例 `localfilesystem:/run/adb/adb.sock`）/ `SPINDLE_TEST_ADB` が要る
#[test]
#[ignore]
fn real_device_sync_round_trip() {
    let Ok(serial) = std::env::var("SPINDLE_TEST_ADB_SERIAL") else {
        return;
    };
    block_on(async {
        let home = std::env::temp_dir().join("spindle-adb-home");
        std::fs::create_dir_all(&home).unwrap();
        let cfg = AdbConfig {
            program: std::env::var("SPINDLE_TEST_ADB")
                .unwrap_or_else(|_| "adb".into())
                .into(),
            server: std::env::var("ADB_SERVER_SOCKET").unwrap(),
            home,
            timeout: Duration::from_secs(60),
            transfer_timeout: Duration::from_secs(600),
        };
        let root = "/storage/emulated/0/Music/spindle-test";
        let fs = AdbFs::new(cfg, &serial, root, CancellationToken::new()).unwrap();
        fs.discard_init().await.unwrap();
        let _ = fs.remove("Dir/Song.opus").await;
        spindle::device::store::initialize(&fs, "u1", "emulated")
            .await
            .unwrap();
        device_support::run_want(&fs, &[(1, "Dir/Song.opus", b"S1"), (2, "x.opus", b"X")])
            .await
            .unwrap();
        device_support::run_want(&fs, &[(1, "Dir/song.opus", b"S1")])
            .await
            .unwrap();
        let files: Vec<String> = fs
            .list_files()
            .await
            .unwrap()
            .into_iter()
            .map(|f| f.path)
            .filter(|p| !p.starts_with(".spindle"))
            .collect();
        assert_eq!(files, vec!["Dir/song.opus".to_string()]);
        fs.remove("Dir/song.opus").await.unwrap();
        fs.discard_init().await.unwrap();
    });
}

#[test]
fn probe_version_reads_the_platform_tools_version() {
    block_on(async {
        let e = env();
        let cfg = e.cfg.clone();
        assert_eq!(
            spindle::device::adb::probe_version(&cfg).await.as_deref(),
            Some("37.0.1-15733141")
        );
    });
}
