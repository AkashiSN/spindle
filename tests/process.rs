//! 外部コマンドの共通ラッパ（SPEC §5「パスの表現と境界」、コーディング規約）。
//! 引数配列、`--` / `./` 前置、タイムアウト、終了コード検査、stderr の保持。
//! 受け入れ: docs/TASKS.md P0-5 (i) `-x.flac` を外部コマンドへ安全に渡せる

use std::fs;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use spindle::jobs::process::{ExternalCommand, PathStyle, ProcessError};

fn dir_with_dash_file() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("-x.flac"), b"dash-content").unwrap();
    dir
}

#[tokio::test]
async fn leading_dash_path_is_passed_after_double_dash() {
    let dir = dir_with_dash_file();
    let out = ExternalCommand::new("cat")
        .current_dir(dir.path())
        .path_arg("-x.flac")
        .run(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out.stdout, b"dash-content");
    assert!(out.status.success());
}

#[tokio::test]
async fn leading_dash_path_is_prefixed_with_dot_slash_for_tools_without_double_dash() {
    let dir = dir_with_dash_file();
    let cmd = ExternalCommand::new("cat")
        .path_style(PathStyle::DotSlash)
        .current_dir(dir.path())
        .path_arg("-x.flac");
    assert_eq!(cmd.arg_list(), ["./-x.flac"]);
    let out = cmd.run(&CancellationToken::new()).await.unwrap();
    assert_eq!(out.stdout, b"dash-content");
}

#[tokio::test]
async fn double_dash_is_emitted_once_before_first_path() {
    let cmd = ExternalCommand::new("flac")
        .arg("-d")
        .path_arg("a.flac")
        .path_arg("-b.flac");
    assert_eq!(cmd.arg_list(), ["-d", "--", "a.flac", "-b.flac"]);
    // 絶対パスは `./` 前置の対象外
    let cmd = ExternalCommand::new("ffmpeg")
        .path_style(PathStyle::DotSlash)
        .arg("-i")
        .path_arg("/abs/-x.flac")
        .path_arg("x.flac");
    assert_eq!(cmd.arg_list(), ["-i", "/abs/-x.flac", "x.flac"]);
}

#[tokio::test]
async fn nonzero_exit_is_an_error_with_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let err = ExternalCommand::new("cat")
        .current_dir(dir.path())
        .path_arg("missing.flac")
        .run(&CancellationToken::new())
        .await
        .unwrap_err();
    match err {
        ProcessError::Failed {
            program, stderr, ..
        } => {
            assert_eq!(program, "cat");
            assert!(stderr.contains("missing.flac"), "{stderr}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn timeout_kills_the_process_group() {
    let started = Instant::now();
    let err = ExternalCommand::new("sleep")
        .arg("60")
        .timeout(Duration::from_millis(200))
        .run(&CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ProcessError::Timeout { .. }), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn cancellation_token_stops_the_process() {
    let token = CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(100)).await;
        t.cancel();
    });
    let err = ExternalCommand::new("sleep")
        .arg("60")
        .run(&token)
        .await
        .unwrap_err();
    assert!(matches!(err, ProcessError::Cancelled), "{err:?}");
}

#[tokio::test]
async fn stdin_can_be_an_opened_file() {
    let dir = dir_with_dash_file();
    let file = fs::File::open(dir.path().join("-x.flac")).unwrap();
    let out = ExternalCommand::new("cat")
        .stdin_file(file)
        .run(&CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out.stdout, b"dash-content");
}

#[tokio::test]
async fn missing_program_is_a_spawn_error() {
    let err = ExternalCommand::new("/nonexistent/spindle-no-such-tool")
        .run(&CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(err, ProcessError::Spawn { .. }), "{err:?}");
}

#[tokio::test]
async fn descendants_left_by_an_exited_leader_are_killed_and_do_not_block_completion() {
    // leader が孫を残して先に exit する。孫は stderr パイプを継承しているので、グループを
    // 掃除しないと EOF が来ず、タイムアウトまで固まる（あるいは孫が残る）
    let started = Instant::now();
    let out = ExternalCommand::new("sh")
        .args(["-c", "sleep 60 & echo $!; exit 0"])
        .timeout(Duration::from_secs(20))
        .run(&CancellationToken::new())
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "leader 終了後にグループ掃除が走るべき: {:?}",
        started.elapsed()
    );
    let pid: i32 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
    let pid = rustix::process::Pid::from_raw(pid).unwrap();
    for _ in 0..100 {
        if rustix::process::test_kill_process(pid).is_err() {
            return; // ESRCH: 孫は消えた
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("leader 終了後も孫 {pid:?} が残っている");
}

#[tokio::test]
async fn stderr_is_bounded_to_a_tail_while_reading() {
    // 16 KiB を大きく超える stderr を出しても末尾だけ保持する
    let err = ExternalCommand::new("sh")
        .args([
            "-c",
            "i=0; while [ $i -lt 20000 ]; do echo \"line-$i-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\"; i=$((i+1)); done >&2; echo END >&2; exit 3",
        ])
        .run(&CancellationToken::new())
        .await
        .unwrap_err();
    let ProcessError::Failed { stderr, .. } = err else {
        panic!("{err:?}");
    };
    assert!(stderr.len() <= 16 * 1024, "{}", stderr.len());
    assert!(
        stderr.ends_with("END"),
        "末尾を保持する: ...{}",
        &stderr[stderr.len() - 40..]
    );
    assert!(stderr.contains("line-19999-"));
    assert!(!stderr.contains("line-0-"));
}

#[tokio::test]
async fn stdout_can_be_streamed_through_a_channel() {
    // 大きな stdout をメモリに溜めず、チャンクのまま受け取れる（ffmpeg の PCM 出力用）
    let dir = tempfile::tempdir().unwrap();
    let big: Vec<u8> = (0..(3 * 1024 * 1024u32)).map(|i| (i % 251) as u8).collect();
    fs::write(dir.path().join("big.bin"), &big).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
    let consumer = tokio::spawn(async move {
        let mut got = Vec::new();
        while let Some(chunk) = rx.recv().await {
            got.extend_from_slice(&chunk);
        }
        got
    });
    let out = ExternalCommand::new("cat")
        .stdin_file(fs::File::open(dir.path().join("big.bin")).unwrap())
        .stdout_channel(tx)
        .run(&CancellationToken::new())
        .await
        .unwrap();
    assert!(
        out.stdout.is_empty(),
        "チャネルへ流した stdout はメモリに残さない"
    );
    assert_eq!(consumer.await.unwrap(), big);
}

#[tokio::test]
async fn dropped_stdout_receiver_stops_the_command() {
    // 受け手が途中で消えたら子をグループごと止める。SIGPIPE を無視して書き続ける子でも
    // タイムアウトまで待たない
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    drop(rx);
    let started = Instant::now();
    let err = ExternalCommand::new("sh")
        .args([
            "-c",
            "trap '' PIPE; while :; do echo x 2>/dev/null || :; done",
        ])
        .stdout_channel(tx)
        .timeout(Duration::from_secs(10))
        .run(&CancellationToken::new())
        .await
        .expect_err("失敗するはず");
    assert!(started.elapsed() < Duration::from_secs(5), "{err:?}");
    assert!(
        matches!(err, ProcessError::OutputAbandoned { .. }),
        "{err:?}"
    );
}
