//! `web/dist` が無くても `cargo build` が通るように空ディレクトリを用意する
//! （`rust-embed` はフォルダの存在をコンパイル時に要求する）。
//! 同梱する SPA は `cd web && npm run build` で生成する。
//! 版の文字列（`SPINDLE_VERSION`。P4-12）もここで確定する

fn main() {
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/dist");
    if !dist.is_dir() {
        // 作れなくても rust-embed 側で分かりやすく失敗するので、ここでは無視する
        let _ = std::fs::create_dir_all(&dist);
    }
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-changed=db/migrations");
    // 版: 環境変数（CI / build.sh が git describe を渡す）> git describe（作業ツリーに .git がある）> dev。
    // 空白を含まない 1 語にする（/health と --version がそのまま出す）。git の状態が変わったら再計算する:
    // HEAD（detached / branch の切り替え）、HEAD が指す ref（commit）、packed-refs（tag / pack 後）、index
    // （stage の変化）。`--dirty` は付けない（作業ツリーの変化は監視できず古い値を名乗るため）
    println!("cargo:rerun-if-env-changed=SPINDLE_VERSION");
    for p in git_watch_paths() {
        println!("cargo:rerun-if-changed={p}");
    }
    let version = std::env::var("SPINDLE_VERSION")
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
        .or_else(git_describe)
        .unwrap_or_else(|| "dev".to_owned());
    let version: String = version
        .chars()
        .map(|c| if c.is_whitespace() { '-' } else { c })
        .collect();
    println!("cargo:rustc-env=SPINDLE_VERSION={version}");
}

/// 再実行の監視対象（.git が無ければ空）
fn git_watch_paths() -> Vec<String> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let git = root.join(".git");
    if !git.exists() {
        return Vec::new();
    }
    let mut out = vec![
        ".git/HEAD".to_owned(),
        ".git/packed-refs".to_owned(),
        ".git/index".to_owned(),
    ];
    if let Ok(head) = std::fs::read_to_string(git.join("HEAD")) {
        if let Some(r) = head.trim().strip_prefix("ref: ") {
            out.push(format!(".git/{r}"));
        }
    }
    out
}

fn git_describe() -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["describe", "--tags", "--always"])
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_owned();
    (!s.is_empty()).then_some(s)
}
