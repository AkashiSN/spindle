//! `web/dist` が無くても `cargo build` が通るように空ディレクトリを用意する
//! （`rust-embed` はフォルダの存在をコンパイル時に要求する）。
//! 同梱する SPA は `cd web && npm run build` で生成する

fn main() {
    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("web/dist");
    if !dist.is_dir() {
        // 作れなくても rust-embed 側で分かりやすく失敗するので、ここでは無視する
        let _ = std::fs::create_dir_all(&dist);
    }
    println!("cargo:rerun-if-changed=web/dist");
    println!("cargo:rerun-if-changed=db/migrations");
}
