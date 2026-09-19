//! Docker の build stage がバイナリに埋め込むファイルを漏れなく COPY しているか。
//! `include_str!` / `include_bytes!` / rust-embed の `#[folder]` はビルド時にリポジトリのファイルを
//! 要求するので、deploy/Dockerfile の COPY 集合に無いと（ローカルでは通るのに）クリーンビルドが落ちる

use std::path::{Component, Path, PathBuf};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// リポジトリ相対に正規化（`..` を畳む）
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            rust_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

/// `src/` 内の埋め込み参照（リポジトリ相対パス）
fn embedded_paths() -> Vec<PathBuf> {
    let root = repo();
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    let mut out = Vec::new();
    for f in files {
        let text = std::fs::read_to_string(&f).unwrap();
        let dir = f.parent().unwrap();
        for line in text.lines() {
            for key in ["include_str!(\"", "include_bytes!(\""] {
                if let Some(i) = line.find(key) {
                    let rest = &line[i + key.len()..];
                    let end = rest.find('"').unwrap();
                    out.push(normalize(
                        dir.join(&rest[..end]).strip_prefix(&root).unwrap(),
                    ));
                }
            }
            if let Some(i) = line.find("#[folder = \"") {
                let rest = &line[i + "#[folder = \"".len()..];
                let end = rest.find('"').unwrap();
                // rust-embed の folder は CARGO_MANIFEST_DIR 相対
                out.push(normalize(Path::new(&rest[..end])));
            }
        }
    }
    out
}

/// Dockerfile の build stage が COPY する宛先（/src 相対）
fn copied_paths() -> Vec<PathBuf> {
    let text = std::fs::read_to_string(repo().join("deploy/Dockerfile")).unwrap();
    let mut out = Vec::new();
    let mut in_build = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with("FROM ") {
            in_build = line.contains(" AS build");
            continue;
        }
        if !in_build || !line.starts_with("COPY ") {
            continue;
        }
        // COPY [--from=x] src... dest。dest は最後の引数（/src 相対）
        let args: Vec<&str> = line["COPY ".len()..]
            .split_whitespace()
            .filter(|a| !a.starts_with("--"))
            .collect();
        let dest = args.last().unwrap();
        if *dest == "./" || *dest == "." {
            for s in &args[..args.len() - 1] {
                out.push(PathBuf::from(s));
            }
        } else {
            out.push(PathBuf::from(dest.trim_end_matches('/')));
        }
    }
    out
}

#[test]
fn every_embedded_file_is_copied_into_the_docker_build_stage() {
    let copied = copied_paths();
    assert!(!copied.is_empty(), "build stage の COPY が読めない");
    let mut missing = Vec::new();
    for p in embedded_paths() {
        assert!(
            repo().join(&p).exists(),
            "埋め込み元が無い: {}",
            p.display()
        );
        let covered = copied.iter().any(|c| p.starts_with(c) || p == *c);
        if !covered {
            missing.push(p);
        }
    }
    assert!(
        missing.is_empty(),
        "deploy/Dockerfile の build stage に COPY されていない埋め込み元: {missing:?}（COPY: {copied:?}）"
    );
}
