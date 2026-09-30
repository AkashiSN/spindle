use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use spindle::device::adb::AdbConfig;
use spindle::device::runtime::{spawn_watcher, AdbRuntime, WatchOptions};
use spindle::device::track::TrackedDevice;
use tokio_util::sync::CancellationToken;

/// `track-devices -l` に `$HOME/frames` の中身を出し、`$HOME/hold` があれば居座る偽の adb
fn fake_adb(dir: &Path) -> PathBuf {
    let p = dir.join("adb");
    std::fs::write(
        &p,
        "#!/bin/sh\n[ \"$ADB_SERVER_SOCKET\" = \"tcp:adb:5037\" ] || exit 1\n\
         [ -d \"$HOME\" ] || exit 134\n\
         if [ \"$1\" = track-devices ] && [ \"$2\" = -l ]; then\n\
           echo run >> \"$HOME/runs\"\n cat \"$HOME/frames\"\n\
           if [ -f \"$HOME/hold\" ]; then sleep 30; fi\n exit 0\nfi\nexit 2\n",
    )
    .unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

fn cfg(dir: &Path) -> AdbConfig {
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    AdbConfig {
        program: fake_adb(dir),
        server: "tcp:adb:5037".into(),
        home,
        timeout: Duration::from_secs(10),
        transfer_timeout: Duration::from_secs(10),
    }
}

fn frame(body: &str) -> String {
    format!("{:04x}{body}", body.len())
}

async fn wait_until(mut f: impl FnMut() -> bool) {
    for _ in 0..200 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("待ったが条件が成り立たない");
}

#[test]
fn apply_reports_newly_connected_only() {
    let rt = AdbRuntime::new(
        cfg(tempfile::tempdir().unwrap().path()),
        CancellationToken::new(),
    );
    let d = |s: &str, st: &str| TrackedDevice {
        serial: s.into(),
        state: st.into(),
        model: None,
    };
    assert_eq!(rt.apply(vec![d("A", "offline")]), Vec::<String>::new());
    assert_eq!(
        rt.apply(vec![d("A", "device"), d("B", "unauthorized")]),
        vec!["A".to_string()]
    );
    assert_eq!(rt.apply(vec![d("A", "device")]), Vec::<String>::new());
    assert!(rt.is_connected("A"));
    assert!(!rt.is_connected("B"));
    rt.lost();
    assert!(rt.devices().is_empty());
    assert_eq!(rt.apply(vec![d("A", "device")]), vec!["A".to_string()]);
}

#[test]
fn device_lock_is_shared_per_device() {
    let rt = AdbRuntime::new(
        cfg(tempfile::tempdir().unwrap().path()),
        CancellationToken::new(),
    );
    let a = rt.device_lock(1);
    let b = rt.device_lock(1);
    let c = rt.device_lock(2);
    assert!(Arc::ptr_eq(&a, &b));
    assert!(!Arc::ptr_eq(&a, &c));
    let _g = a.try_lock().unwrap();
    assert!(b.try_lock().is_err());
    assert!(c.try_lock().is_ok());
}

#[tokio::test]
async fn watcher_sees_devices_and_calls_back_then_reconnects() {
    let dir = tempfile::tempdir().unwrap();
    let c = cfg(dir.path());
    std::fs::write(
        c.home.join("frames"),
        frame("SER1             device usb:1-4 model:XQ_CC44 transport_id:1\n"),
    )
    .unwrap();
    let shutdown = CancellationToken::new();
    let rt = AdbRuntime::new(c.clone(), shutdown.clone());
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let s2 = Arc::clone(&seen);
    let h = spawn_watcher(
        Arc::clone(&rt),
        WatchOptions {
            min_backoff: Duration::from_millis(50),
            max_backoff: Duration::from_millis(100),
        },
        Arc::new(move |serial: String| {
            let s = Arc::clone(&s2);
            Box::pin(async move { s.lock().unwrap().push(serial) })
        }),
    );
    // 1 回目: 出力して終わる → 張り直す → もう一度 device として見える
    wait_until(|| seen.lock().unwrap().len() >= 2).await;
    assert!(seen.lock().unwrap().iter().all(|s| s == "SER1"));
    let runs = std::fs::read_to_string(c.home.join("runs")).unwrap();
    assert!(runs.lines().count() >= 2);
    shutdown.cancel();
    tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn watcher_stops_a_long_running_track_devices_on_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let c = cfg(dir.path());
    std::fs::write(c.home.join("frames"), frame("SER1\tdevice\n")).unwrap();
    std::fs::write(c.home.join("hold"), b"").unwrap();
    let shutdown = CancellationToken::new();
    let rt = AdbRuntime::new(c, shutdown.clone());
    let h = spawn_watcher(
        Arc::clone(&rt),
        WatchOptions::default(),
        Arc::new(|_s: String| Box::pin(async {})),
    );
    wait_until(|| rt.is_connected("SER1")).await;
    shutdown.cancel();
    // sleep 30 の子を待たずに終わる（プロセスグループごと kill）
    tokio::time::timeout(Duration::from_secs(5), h)
        .await
        .unwrap()
        .unwrap();
}
