//! チャンクの受け取り（設計 §5.2）。
//!
//! - 最初に [`begin_origin`] で origin を作り（宣言サイズを予算に算入）、`files/<origin>.part` を作る
//! - 1 チャンクは [`receive_chunk`]: マニフェストの鍵の中で `.part` の実際の大きさを `offset` と照らし、
//!   追記 → fsync → `received` を実際の大きさに。最後のチャンクで正式な名前（zip は
//!   `zips/<origin>.zip`、それ以外は `files/<origin>`）へ `RENAME_NOREPLACE`
//! - **`.part` の大きさが正**で、マニフェストの `received` は表示用の写し。電源断でマニフェストが
//!   遅れても [`sync_with_disk`] が実体に合わせる
//! - アップロードのコードは unlink しない（`offset = 0` の `O_TRUNC` は同じファイルの書き直し）

use std::io::Write;

use crate::config::UploadConfig;
use crate::domain::relpath::{canonical_key, RelPath, MAX_COMPONENT_BYTES};
use crate::fsroot::{FileKind, FsError, RootDir};

use super::budget::check_budget;
use super::manifest::{
    classify, with_manifest, FileClass, Manifest, Origin, UploadError, UploadId, UploadLocks,
};

/// [`receive_chunk`] の結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChunkResult {
    /// `.part`（完了後は正式な名前）の実際の大きさ
    pub received: u64,
    pub complete: bool,
}

/// 障害注入の位置（テスト用。本番は `&|_| false`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChunkStep {
    /// 追記と fsync の後、マニフェストを書く前
    AfterAppend,
    /// 正式な名前への rename の後、マニフェストを書く前
    AfterRename,
}

/// ブラウザ上の相対パスを検証する。`\` を `/` に読み替えない
pub fn validate_client_path(raw: &str) -> Result<RelPath, UploadError> {
    let bad = |why: &str| Err(UploadError::BadPath(format!("{why}: {raw:?}")));
    if raw.contains('\\') {
        return bad("`\\` を含む");
    }
    if raw.starts_with('/') {
        return bad("絶対パス");
    }
    if raw.chars().any(char::is_control) {
        return bad("制御文字を含む");
    }
    for component in raw.split('/') {
        if component.is_empty() {
            return bad("空の要素を含む");
        }
        // `.`・`..` もここで弾く
        if component.starts_with('.') {
            return bad("`.` で始まる要素を含む");
        }
        if component.len() > MAX_COMPONENT_BYTES {
            return bad("要素が長すぎる");
        }
    }
    Ok(RelPath::parse(raw)?)
}

/// 公開の要求後・破棄済みのアップロードは変えない
fn ensure_open(m: &Manifest) -> Result<(), UploadError> {
    if m.publish_requested_at.is_some() || m.discarded_at.is_some() {
        return Err(UploadError::Busy);
    }
    Ok(())
}

/// 受け取り終えた実体の置き場（zip は `zips/<id>.zip`、それ以外は `files/<id>`）
fn final_path(o: &Origin) -> Result<RelPath, UploadError> {
    if o.class == FileClass::Zip {
        o.zip_path()
    } else {
        o.file_path()
    }
}

fn fault_error() -> UploadError {
    UploadError::Io(std::io::Error::other("fault"))
}

/// rename の後に親ディレクトリを fsync してエントリを永続化する
fn fsync_parent(inbox: &RootDir, rel: &RelPath) -> Result<(), UploadError> {
    inbox.fsync_dir(rel.parent().as_ref())?;
    Ok(())
}

/// 送るファイル 1 つの origin を作る（同じパスの未完の origin があればそれを返して作り直す）
pub fn begin_origin(
    inbox: &RootDir,
    locks: &UploadLocks,
    id: &UploadId,
    client_path: &str,
    total: u64,
    cfg: &UploadConfig,
    now: i64,
) -> Result<Origin, UploadError> {
    let rel = validate_client_path(client_path)?;
    let class = classify(rel.as_str());
    if total > cfg.max_file_bytes {
        return Err(UploadError::Limit(format!(
            "1 ファイルの上限 {} バイトを超える",
            cfg.max_file_bytes
        )));
    }
    let key = canonical_key(rel.as_str());
    with_manifest(inbox, locks, id, |m| {
        ensure_open(m)?;
        sync_with_disk(inbox, m)?;

        // 同じパスの origin: 未完ならドロップし直し、完了済みなら既にある、外した・壊れたなら新しく作る
        let mut reuse = None;
        for (i, o) in m.origins.iter().enumerate() {
            if o.removed || o.damaged || canonical_key(&o.client_path) != key {
                continue;
            }
            if o.complete {
                return Err(UploadError::Exists(client_path.to_string()));
            }
            reuse = Some(i);
            break;
        }

        // 作り直す origin の古い宣言サイズと .part は置き換わるので数えない
        let reclaim = match reuse {
            Some(i) => {
                let o = &mut m.origins[i];
                o.total = 0;
                o.received
            }
            None => 0,
        };
        check_budget(m, total, cfg)?;
        if inbox.free_bytes()?.saturating_add(reclaim) < total {
            return Err(UploadError::NoSpace);
        }

        let index = match reuse {
            Some(i) => i,
            None => {
                m.next_origin = m
                    .next_origin
                    .checked_add(1)
                    .ok_or_else(|| UploadError::Limit("origin の数".into()))?;
                m.origins.push(Origin {
                    id: format!("o{}", m.next_origin),
                    client_path: String::new(),
                    class,
                    total: 0,
                    received: 0,
                    complete: false,
                    removed: false,
                    damaged: false,
                    zip: None,
                });
                m.origins.len() - 1
            }
        };
        let o = &mut m.origins[index];
        o.client_path = rel.as_str().to_string();
        o.class = class;
        o.total = total;
        o.received = 0;

        let part = id.staged(&o.part_path()?)?;
        inbox.create_truncate(&part)?.sync_all()?;
        fsync_parent(inbox, &part)?;
        let out = o.clone();
        m.updated_at = now;
        Ok(out)
    })
}

/// チャンク 1 つを受け取る
#[allow(clippy::too_many_arguments)]
pub fn receive_chunk(
    inbox: &RootDir,
    locks: &UploadLocks,
    id: &UploadId,
    origin: &str,
    offset: u64,
    bytes: &[u8],
    now: i64,
    fault: &dyn Fn(ChunkStep) -> bool,
) -> Result<ChunkResult, UploadError> {
    with_manifest(inbox, locks, id, |m| {
        ensure_open(m)?;
        sync_with_disk(inbox, m)?;
        let o = m
            .origins
            .iter_mut()
            .find(|o| o.id == origin)
            .ok_or(UploadError::NotFound)?;
        if o.complete {
            return Err(UploadError::Conflict("受け取り済み".into()));
        }
        if o.damaged {
            return Err(UploadError::Conflict("壊れた。外して送り直す".into()));
        }
        if o.removed {
            return Err(UploadError::Conflict("外したファイル".into()));
        }
        let len = u64::try_from(bytes.len()).map_err(|_| UploadError::Limit("チャンク".into()))?;
        if offset.checked_add(len).is_none_or(|end| end > o.total) {
            return Err(UploadError::Limit(format!(
                "宣言サイズ {} バイトを超える",
                o.total
            )));
        }

        // .part の実際の大きさが正
        let part = id.staged(&o.part_path()?)?;
        let mut file = if offset == 0 {
            inbox.create_truncate(&part)?
        } else {
            let file = inbox.open_append(&part)?;
            let size = file.metadata()?.len();
            if size != offset {
                return Err(UploadError::Offset { received: size });
            }
            file
        };
        file.write_all(bytes)?;
        file.sync_all()?;
        if fault(ChunkStep::AfterAppend) {
            return Err(fault_error());
        }
        let size = file.metadata()?.len();
        drop(file);
        o.received = size;

        if size == o.total {
            let dest = id.staged(&final_path(o)?)?;
            inbox.rename_noreplace(&part, &dest)?;
            fsync_parent(inbox, &dest)?;
            if part.parent() != dest.parent() {
                fsync_parent(inbox, &part)?;
            }
            if fault(ChunkStep::AfterRename) {
                return Err(fault_error());
            }
            o.complete = true;
        }
        let out = ChunkResult {
            received: size,
            complete: o.complete,
        };
        m.updated_at = now;
        Ok(out)
    })
}

/// 実体の有無と大きさ
enum Present {
    Missing,
    File(u64),
    /// ファイルでないもの（ディレクトリ・symlink など）
    Other,
}

fn probe(inbox: &RootDir, rel: &RelPath) -> Result<Present, UploadError> {
    match inbox.stat(rel) {
        Ok(st) if st.kind == FileKind::File => Ok(Present::File(st.size)),
        Ok(_) => Ok(Present::Other),
        Err(FsError::NotFound) => Ok(Present::Missing),
        Err(e) => Err(e.into()),
    }
}

/// 未完の origin を実体に合わせる（呼び出し側が鍵を持つ）。変えたら true。
///
/// `.part` だけがあれば `received` をその大きさに、`.part` が無く正式な名前の大きさが `total` なら
/// 完了に進める（rename の後・マニフェストを書く前に落ちた場合）。それ以外の食い違いは `damaged`
/// にする（実体はどちらも残し、GC がステージングごと消す）
pub fn sync_with_disk(inbox: &RootDir, m: &mut Manifest) -> Result<bool, UploadError> {
    let id = UploadId::parse(&m.id)?;
    let mut changed = false;
    for o in m
        .origins
        .iter_mut()
        .filter(|o| !o.complete && !o.damaged && !o.removed)
    {
        let part = probe(inbox, &id.staged(&o.part_path()?)?)?;
        let done = probe(inbox, &id.staged(&final_path(o)?)?)?;
        match (part, done) {
            (Present::File(size), Present::Missing) if size <= o.total => {
                if o.received != size {
                    o.received = size;
                    changed = true;
                }
            }
            (Present::Missing, Present::File(size)) if size == o.total => {
                o.received = size;
                o.complete = true;
                changed = true;
            }
            _ => {
                tracing::warn!(
                    upload = id.as_str(),
                    origin = o.id.as_str(),
                    "アップロードの実体がマニフェストと食い違うので壊れたとする"
                );
                o.damaged = true;
                changed = true;
            }
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::import::upload::manifest::{create_upload, read_manifest, ZipState};
    use std::path::PathBuf;
    use std::sync::{Arc, Barrier};
    use std::thread;

    struct Env {
        dir: tempfile::TempDir,
        inbox: RootDir,
        locks: UploadLocks,
        id: UploadId,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let inbox = RootDir::open(dir.path()).unwrap();
            let m = create_upload(&inbox, 1).unwrap();
            let id = UploadId::parse(&m.id).unwrap();
            Self {
                dir,
                inbox,
                locks: UploadLocks::default(),
                id,
            }
        }

        /// ステージング内の相対パスの実パス
        fn path(&self, rel: &str) -> PathBuf {
            self.dir
                .path()
                .join(self.id.staging_dir().as_str())
                .join(rel)
        }

        fn begin(&self, path: &str, total: u64, cfg: &UploadConfig) -> Result<Origin, UploadError> {
            begin_origin(&self.inbox, &self.locks, &self.id, path, total, cfg, 10)
        }

        fn put(&self, origin: &str, offset: u64, bytes: &[u8]) -> Result<ChunkResult, UploadError> {
            receive_chunk(
                &self.inbox,
                &self.locks,
                &self.id,
                origin,
                offset,
                bytes,
                20,
                &|_| false,
            )
        }

        fn manifest(&self) -> Manifest {
            read_manifest(&self.inbox, &self.id).unwrap()
        }

        fn origin(&self, oid: &str) -> Origin {
            self.manifest()
                .origins
                .into_iter()
                .find(|o| o.id == oid)
                .unwrap()
        }

        fn update(&self, f: impl FnOnce(&mut Manifest)) {
            with_manifest(&self.inbox, &self.locks, &self.id, |m| {
                f(m);
                Ok(())
            })
            .unwrap();
        }

        fn sync(&self) -> bool {
            with_manifest(&self.inbox, &self.locks, &self.id, |m| {
                sync_with_disk(&self.inbox, m)
            })
            .unwrap()
        }
    }

    fn cfg() -> UploadConfig {
        UploadConfig::default()
    }

    #[test]
    fn two_chunks_complete_file() {
        let env = Env::new();
        let o = env.begin("Album/01.flac", 6, &cfg()).unwrap();
        assert_eq!(o.id, "o1");
        assert_eq!(o.class, FileClass::Audio);
        assert!(env.path("files/o1.part").is_file());
        let r = env.put("o1", 0, b"abc").unwrap();
        assert_eq!(
            r,
            ChunkResult {
                received: 3,
                complete: false
            }
        );
        assert_eq!(env.origin("o1").received, 3);
        let r = env.put("o1", 3, b"def").unwrap();
        assert_eq!(
            r,
            ChunkResult {
                received: 6,
                complete: true
            }
        );
        assert!(!env.path("files/o1.part").exists());
        assert_eq!(std::fs::read(env.path("files/o1")).unwrap(), b"abcdef");
        let o = env.origin("o1");
        assert!(o.complete);
        assert_eq!(o.received, 6);
        assert_eq!(env.manifest().updated_at, 20);
        // 完了済みへの PUT は Conflict
        assert!(matches!(
            env.put("o1", 0, b"x"),
            Err(UploadError::Conflict(_))
        ));
        // 宣言サイズを超えるチャンクは Limit
        env.begin("Album/02.flac", 4, &cfg()).unwrap();
        assert!(matches!(
            env.put("o2", 0, b"12345"),
            Err(UploadError::Limit(_))
        ));
        // 無い origin は NotFound
        assert!(matches!(env.put("o9", 0, b"x"), Err(UploadError::NotFound)));
    }

    #[test]
    fn offset_mismatch_returns_actual_size() {
        let env = Env::new();
        env.begin("a.flac", 10, &cfg()).unwrap();
        env.put("o1", 0, b"abcde").unwrap();
        // マニフェストの写しを古くしておく
        env.update(|m| m.origins[0].received = 2);
        match env.put("o1", 2, b"xyz") {
            Err(UploadError::Offset { received }) => assert_eq!(received, 5),
            other => panic!("{other:?}"),
        }
        match env.put("o1", 7, b"xyz") {
            Err(UploadError::Offset { received }) => assert_eq!(received, 5),
            other => panic!("{other:?}"),
        }
        assert_eq!(std::fs::read(env.path("files/o1.part")).unwrap(), b"abcde");
        // 正しい offset なら続けられる
        assert_eq!(env.put("o1", 5, b"fg").unwrap().received, 7);
    }

    #[test]
    fn offset_zero_restarts_part() {
        let env = Env::new();
        env.begin("a.flac", 6, &cfg()).unwrap();
        env.put("o1", 0, b"abcd").unwrap();
        let r = env.put("o1", 0, b"XY").unwrap();
        assert_eq!(r.received, 2);
        assert_eq!(std::fs::read(env.path("files/o1.part")).unwrap(), b"XY");
        assert_eq!(env.origin("o1").received, 2);
    }

    #[test]
    fn rejects_bad_paths() {
        let long = "a".repeat(256);
        let long_path = format!("Album/{long}.flac");
        for bad in [
            "../a.flac",
            "/a.flac",
            "a/./b.flac",
            ".hidden/a.flac",
            "a\\b.flac",
            long_path.as_str(),
            "",
            "a//b.flac",
            "a/\u{1}b.flac",
            "Album/.flac",
        ] {
            assert!(
                matches!(validate_client_path(bad), Err(UploadError::BadPath(_))),
                "{bad:?}"
            );
        }
        assert_eq!(
            validate_client_path("Album/Disc 1/01 曲.flac")
                .unwrap()
                .as_str(),
            "Album/Disc 1/01 曲.flac"
        );
        let ok255 = format!("{}.flac", "a".repeat(250));
        assert!(validate_client_path(&ok255).is_ok());
        // begin_origin も検証を通す
        let env = Env::new();
        assert!(matches!(
            env.begin("../a.flac", 1, &cfg()),
            Err(UploadError::BadPath(_))
        ));
        assert!(env.manifest().origins.is_empty());
    }

    #[test]
    fn rejects_case_only_duplicate() {
        let env = Env::new();
        env.begin("Album/01.flac", 2, &cfg()).unwrap();
        env.put("o1", 0, b"ab").unwrap();
        assert!(matches!(
            env.begin("album/01.FLAC", 2, &cfg()),
            Err(UploadError::Exists(_))
        ));
        assert_eq!(env.manifest().origins.len(), 1);
    }

    #[test]
    fn budget_counts_receiving_and_unpacked() {
        let env = Env::new();
        let cfg = UploadConfig {
            max_upload_bytes: 100,
            max_file_bytes: 80,
            ..UploadConfig::default()
        };
        // 受信中の宣言サイズ 60
        env.begin("a.flac", 60, &cfg).unwrap();
        // 展開済みの zip（宣言 5 + 書いた 25）
        env.begin("m.zip", 5, &cfg).unwrap();
        env.put("o2", 0, b"12345").unwrap();
        env.update(|m| {
            m.origins[1].zip = Some(ZipState {
                generation: 1,
                next_generation: 2,
                written_bytes: 25,
                ..ZipState::default()
            })
        });
        // 60 + 5 + 25 + 11 = 101 > 100
        assert!(matches!(
            env.begin("b.flac", 11, &cfg),
            Err(UploadError::Limit(_))
        ));
        env.begin("b.flac", 10, &cfg).unwrap();
        // 1 ファイルの上限
        let env2 = Env::new();
        assert!(matches!(
            env2.begin("c.flac", 81, &cfg),
            Err(UploadError::Limit(_))
        ));
    }

    #[test]
    fn no_space_when_free_bytes_too_small() {
        let env = Env::new();
        let cfg = UploadConfig {
            max_file_bytes: u64::MAX,
            max_upload_bytes: u64::MAX,
            ..UploadConfig::default()
        };
        assert!(matches!(
            env.begin("huge.flac", u64::MAX / 2, &cfg),
            Err(UploadError::NoSpace)
        ));
    }

    #[test]
    fn concurrent_chunks_two_origins() {
        let env = Arc::new(Env::new());
        let cfg = UploadConfig {
            max_upload_bytes: 100,
            ..UploadConfig::default()
        };
        env.begin("a.flac", 40, &cfg).unwrap();
        env.begin("b.flac", 40, &cfg).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = ["o1", "o2"]
            .into_iter()
            .map(|oid| {
                let (env, barrier) = (env.clone(), barrier.clone());
                thread::spawn(move || {
                    barrier.wait();
                    let mut last = None;
                    for i in 0..10u64 {
                        last = Some(env.put(oid, i * 4, &[i as u8; 4]).unwrap());
                    }
                    last.unwrap()
                })
            })
            .collect();
        for h in handles {
            assert!(h.join().unwrap().complete);
        }
        for oid in ["o1", "o2"] {
            assert_eq!(
                std::fs::read(env.path(&format!("files/{oid}")))
                    .unwrap()
                    .len(),
                40
            );
            assert!(env.origin(oid).complete);
        }
        // 予算は合計で数える: 40 + 40 + 21 > 100
        assert!(matches!(
            env.begin("c.flac", 21, &cfg),
            Err(UploadError::Limit(_))
        ));
        env.begin("c.flac", 20, &cfg).unwrap();
    }

    #[test]
    fn concurrent_same_origin_one_wins() {
        let env = Arc::new(Env::new());
        env.begin("a.flac", 10, &cfg()).unwrap();
        env.put("o1", 0, b"abc").unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let handles: Vec<_> = (0..2)
            .map(|_| {
                let (env, barrier) = (env.clone(), barrier.clone());
                thread::spawn(move || {
                    barrier.wait();
                    env.put("o1", 3, b"def")
                })
            })
            .collect();
        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let oks = results.iter().filter(|r| r.is_ok()).count();
        let offsets: Vec<_> = results
            .iter()
            .filter_map(|r| match r {
                Err(UploadError::Offset { received }) => Some(*received),
                _ => None,
            })
            .collect();
        assert_eq!(oks, 1, "{results:?}");
        assert_eq!(offsets, vec![6]);
        assert_eq!(std::fs::read(env.path("files/o1.part")).unwrap(), b"abcdef");
    }

    #[test]
    fn zip_completes_into_zips_dir() {
        let env = Env::new();
        let o = env.begin("mora.zip", 4, &cfg()).unwrap();
        assert_eq!(o.class, FileClass::Zip);
        assert!(env.put("o1", 0, b"PK..").unwrap().complete);
        assert_eq!(std::fs::read(env.path("zips/o1.zip")).unwrap(), b"PK..");
        assert!(!env.path("files/o1.part").exists());
        assert!(!env.path("files/o1").exists());
    }

    #[test]
    fn busy_after_publish_requested() {
        let env = Env::new();
        env.begin("a.flac", 4, &cfg()).unwrap();
        env.update(|m| m.publish_requested_at = Some(5));
        assert!(matches!(env.put("o1", 0, b"ab"), Err(UploadError::Busy)));
        assert!(matches!(
            env.begin("b.flac", 4, &cfg()),
            Err(UploadError::Busy)
        ));
        let env = Env::new();
        env.update(|m| m.discarded_at = Some(5));
        assert!(matches!(
            env.begin("b.flac", 4, &cfg()),
            Err(UploadError::Busy)
        ));
    }

    #[test]
    fn redrop_same_path_reuses_incomplete_origin() {
        let env = Env::new();
        env.begin("Album/01.flac", 6, &cfg()).unwrap();
        env.put("o1", 0, b"abc").unwrap();
        // 大小文字違いでも同じパス。宣言サイズが変わってもよい
        let o = env.begin("album/01.flac", 8, &cfg()).unwrap();
        assert_eq!(o.id, "o1");
        assert_eq!(o.total, 8);
        assert_eq!(o.received, 0);
        assert_eq!(
            std::fs::metadata(env.path("files/o1.part")).unwrap().len(),
            0
        );
        let m = env.manifest();
        assert_eq!(m.origins.len(), 1);
        assert_eq!(m.next_origin, 1);
        assert!(env.put("o1", 0, b"12345678").unwrap().complete);

        // 外した origin と同じパスは新しい origin
        env.begin("Album/02.flac", 2, &cfg()).unwrap();
        env.update(|m| m.origins[1].removed = true);
        let o = env.begin("Album/02.flac", 2, &cfg()).unwrap();
        assert_eq!(o.id, "o3");
    }

    #[test]
    fn crash_after_rename_before_manifest_recovers() {
        let env = Env::new();
        env.begin("a.flac", 4, &cfg()).unwrap();
        env.put("o1", 0, b"ab").unwrap();
        let r = receive_chunk(&env.inbox, &env.locks, &env.id, "o1", 2, b"cd", 20, &|s| {
            s == ChunkStep::AfterRename
        });
        assert!(matches!(r, Err(UploadError::Io(_))));
        // マニフェストは遅れている
        let o = env.origin("o1");
        assert!(!o.complete);
        assert_eq!(o.received, 2);
        assert!(env.path("files/o1").is_file());
        assert!(!env.path("files/o1.part").exists());
        assert!(env.sync());
        let o = env.origin("o1");
        assert!(o.complete);
        assert!(!o.damaged);
        assert_eq!(o.received, 4);
        assert!(!env.sync(), "2 回目は何も変えない");
    }

    #[test]
    fn crash_after_append_resyncs_received() {
        let env = Env::new();
        env.begin("a.flac", 6, &cfg()).unwrap();
        env.put("o1", 0, b"ab").unwrap();
        let r = receive_chunk(&env.inbox, &env.locks, &env.id, "o1", 2, b"cd", 20, &|s| {
            s == ChunkStep::AfterAppend
        });
        assert!(matches!(r, Err(UploadError::Io(_))));
        assert_eq!(env.origin("o1").received, 2);
        assert!(env.sync());
        assert_eq!(env.origin("o1").received, 4);
        // クライアントは Offset で実際の大きさを知って続ける
        match env.put("o1", 2, b"cd") {
            Err(UploadError::Offset { received }) => assert_eq!(received, 4),
            other => panic!("{other:?}"),
        }
        assert!(env.put("o1", 4, b"ef").unwrap().complete);
        assert_eq!(std::fs::read(env.path("files/o1")).unwrap(), b"abcdef");
    }

    #[test]
    fn both_part_and_final_marks_damaged_and_redrop_gets_new_origin() {
        let env = Env::new();
        env.begin("Album/01.flac", 4, &cfg()).unwrap();
        env.put("o1", 0, b"ab").unwrap();
        // 外部の変更で正式な名前も現れた
        std::fs::write(env.path("files/o1"), b"zzzz").unwrap();
        assert!(env.sync());
        let o = env.origin("o1");
        assert!(o.damaged);
        assert!(!o.complete);
        // 実体はどちらも残る
        assert!(env.path("files/o1").is_file());
        assert!(env.path("files/o1.part").is_file());
        assert!(matches!(
            env.put("o1", 2, b"cd"),
            Err(UploadError::Conflict(_))
        ));
        // 同じパスのドロップし直しは新しい origin
        let o = env.begin("Album/01.flac", 4, &cfg()).unwrap();
        assert_eq!(o.id, "o2");
        assert!(env.put("o2", 0, b"abcd").unwrap().complete);

        // .part が宣言サイズより大きい・正式な名前の大きさが合わないのも damaged
        env.begin("b.flac", 2, &cfg()).unwrap();
        std::fs::write(env.path("files/o3.part"), b"toolong").unwrap();
        env.begin("c.flac", 3, &cfg()).unwrap();
        std::fs::rename(env.path("files/o4.part"), env.path("files/o4")).unwrap();
        assert!(env.sync());
        assert!(env.origin("o3").damaged);
        assert!(env.origin("o4").damaged);
        // 完了済みの origin は触らない
        assert!(!env.origin("o2").damaged);
    }
}
