//! アップロードの公開計画の凍結と段階的な公開（`import::upload::publish`。SPEC §7.12「Inbox へ置く（公開）」）。
//! 一時ディレクトリの Inbox に本物の送信（`begin_origin` + `receive_chunk`）でファイルを置き、
//! `freeze` → `publish` で `upload/…` に件ができることと、各地点で止めて再実行しても
//! 同じ結果に 1 回だけなることを確かめる

#![cfg(target_os = "linux")]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::config::{LayoutConfig, UploadConfig};
use spindle::db::open_memory_connection;
use spindle::fsroot::RootDir;
use spindle::import::sidecar::{Sidecar, UploadEntry, SIDECAR_NAME};
use spindle::import::upload::{
    begin_origin, create_upload, freeze, publish, read_manifest, receive_chunk, with_manifest,
    FreezeOutcome, Manifest, PublishReport, PublishStep, Stage, UploadError, UploadId, UploadLocks,
};

const NOW: i64 = 1_700_000_000;

fn layout() -> LayoutConfig {
    LayoutConfig {
        multi_disc: "{category}/{albumartist}/{album}/{disc}-{track:02} {title}".into(),
        single_disc: "{category}/{albumartist}/{album}/{track:02} {title}".into(),
        unsorted: "_Unsorted/{albumartist}/{album}/{track:02} {title}".into(),
    }
}

struct Env {
    dir: tempfile::TempDir,
    inbox: RootDir,
    locks: UploadLocks,
    id: UploadId,
    conn: Connection,
    cfg: UploadConfig,
}

impl Env {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Inbox", "src"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let inbox = RootDir::open(&dir.path().join("Inbox")).unwrap();
        let m = create_upload(&inbox, NOW).unwrap();
        let id = UploadId::parse(&m.id).unwrap();
        Self {
            dir,
            inbox,
            locks: UploadLocks::default(),
            id,
            conn: open_memory_connection().unwrap(),
            cfg: UploadConfig::default(),
        }
    }

    fn inbox_path(&self) -> PathBuf {
        self.dir.path().join("Inbox")
    }

    fn staging_path(&self) -> PathBuf {
        self.inbox_path().join(self.id.staging_dir().as_str())
    }

    /// 送る元の WAV（`album` が Some ならタグを付ける。アルバムアーティストは `Artist`）
    fn wav(&self, name: &str, seed: u32, album: Option<&str>, track: u32) -> Vec<u8> {
        let p = self.dir.path().join("src").join(format!("{seed}-{name}"));
        common::write_wav(&p, &common::pcm_samples(seed), 16);
        if let Some(album) = album {
            common::set_basic_tags(&p, name, "Artist", album, "Artist", track, 1);
        }
        std::fs::read(&p).unwrap()
    }

    fn send(&self, client_path: &str, bytes: &[u8]) -> String {
        let o = begin_origin(
            &self.inbox,
            &self.locks,
            &self.id,
            client_path,
            bytes.len() as u64,
            &self.cfg,
            NOW,
        )
        .unwrap();
        let r = receive_chunk(
            &self.inbox,
            &self.locks,
            &self.id,
            &o.id,
            0,
            bytes,
            NOW,
            &|_| false,
        )
        .unwrap();
        assert!(r.complete);
        o.id
    }

    fn send_partial(&self, client_path: &str, bytes: &[u8]) {
        let o = begin_origin(
            &self.inbox,
            &self.locks,
            &self.id,
            client_path,
            bytes.len() as u64,
            &self.cfg,
            NOW,
        )
        .unwrap();
        receive_chunk(
            &self.inbox,
            &self.locks,
            &self.id,
            &o.id,
            0,
            &bytes[..bytes.len() / 2],
            NOW,
            &|_| false,
        )
        .unwrap();
    }

    fn manifest(&self) -> Manifest {
        read_manifest(&self.inbox, &self.id).unwrap()
    }

    fn edit(&self, f: impl FnOnce(&mut Manifest)) {
        with_manifest(&self.inbox, &self.locks, &self.id, |m| {
            f(m);
            Ok(())
        })
        .unwrap();
    }

    fn freeze(&self) -> Result<FreezeOutcome, UploadError> {
        freeze(
            &self.inbox,
            &self.locks,
            &self.id,
            &self.conn,
            &layout(),
            &self.cfg,
            NOW,
        )
    }

    fn publish_with(
        &self,
        fault: &dyn Fn(PublishStep) -> bool,
    ) -> Result<PublishReport, UploadError> {
        publish(
            &self.inbox,
            &self.locks,
            &self.id,
            NOW,
            &CancellationToken::new(),
            fault,
        )
    }

    fn publish(&self) -> PublishReport {
        self.publish_with(&|_| false).unwrap()
    }

    /// 2 件: タグ付きの `X`（2 曲 + 同梱の cover.jpg）と、ばらの未タグ 1 曲。取り込まない readme も 1 つ
    fn two_groups(&self) {
        let files = self.two_groups_files();
        self.send_all(&files);
    }

    /// [`Env::two_groups`] の送る中身（lofty のタグの書き出しはバイト列が毎回同じとは限らないので、
    /// 環境どうしで結果を比べるときは 1 回作ったものを使い回す）
    fn two_groups_files(&self) -> Vec<(&'static str, Vec<u8>)> {
        vec![
            ("X/01.wav", self.wav("01.wav", 1, Some("X"), 1)),
            ("X/02.wav", self.wav("02.wav", 2, Some("X"), 2)),
            ("X/cover.jpg", b"jpeg".to_vec()),
            ("loose.wav", self.wav("loose.wav", 3, None, 1)),
            ("X/readme.txt", b"hello".to_vec()),
        ]
    }

    fn send_all(&self, files: &[(&str, Vec<u8>)]) {
        for (path, bytes) in files {
            self.send(path, bytes);
        }
    }

    /// Inbox の `upload/` の下の木（パス → 中身）。サイドカーのアップロード ID は環境ごとに違うので伏せる
    fn upload_tree(&self) -> BTreeMap<String, Vec<u8>> {
        let mut out = BTreeMap::new();
        let root = self.inbox_path().join("upload");
        if root.exists() {
            collect(&root, &root, self.id.as_str(), &mut out);
        }
        out
    }
}

fn collect(root: &Path, dir: &Path, id: &str, out: &mut BTreeMap<String, Vec<u8>>) {
    for e in std::fs::read_dir(dir).unwrap() {
        let e = e.unwrap();
        let p = e.path();
        let rel = p.strip_prefix(root).unwrap().to_string_lossy().into_owned();
        if e.file_type().unwrap().is_dir() {
            out.insert(format!("{rel}/"), Vec::new());
            collect(root, &p, id, out);
        } else {
            let mut data = std::fs::read(&p).unwrap();
            if p.file_name().unwrap() == SIDECAR_NAME {
                data = String::from_utf8(data)
                    .unwrap()
                    .replace(id, "ID")
                    .into_bytes();
            }
            out.insert(rel, data);
        }
    }
}

fn names_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    v.sort();
    v
}

fn stages(m: &Manifest) -> Vec<Stage> {
    m.publish
        .as_ref()
        .unwrap()
        .groups
        .iter()
        .map(|g| g.stage.clone())
        .collect()
}

#[test]
fn freeze_is_idempotent() {
    let env = Env::new();
    env.two_groups();
    assert_eq!(env.freeze().unwrap(), FreezeOutcome::Frozen);
    let m = env.manifest();
    let plan = m.publish.clone().unwrap();
    assert_eq!(plan.frozen_at, NOW);
    assert_eq!(m.publish_requested_at, Some(NOW));
    assert_eq!(plan.groups.len(), 2);
    assert!(plan.groups.iter().all(|g| g.stage == Stage::Planned));
    let ns: Vec<u32> = plan.groups.iter().map(|g| g.n).collect();
    assert_eq!(ns, vec![1, 2]);
    let x = plan
        .groups
        .iter()
        .find(|g| g.album.as_deref() == Some("X"))
        .unwrap();
    assert_eq!(x.albumartist, "Artist");
    let mut names: Vec<&str> = x.members.iter().map(|m| m.name.as_str()).collect();
    names.sort();
    assert_eq!(names, vec!["01.wav", "02.wav", "cover.jpg"]);

    // 2 回目は計画を変えない（その間の ③ の変更も効かない）
    with_manifest(&env.inbox, &env.locks, &env.id, |m| {
        m.excluded_groups.push(x.key.clone());
        Ok(())
    })
    .unwrap();
    assert_eq!(env.freeze().unwrap(), FreezeOutcome::AlreadyFrozen);
    let m2 = env.manifest();
    assert_eq!(m2.publish, Some(plan));
    assert_eq!(m2.publish_requested_at, Some(NOW));
}

#[test]
fn freeze_rejects_incomplete_and_empty() {
    // 何も無い
    let env = Env::new();
    assert!(matches!(env.freeze(), Err(UploadError::Conflict(s)) if s == "empty"));
    assert!(env.manifest().publish.is_none());

    // 送信未完
    let env = Env::new();
    env.send("X/01.wav", &env.wav("01.wav", 1, Some("X"), 1));
    env.send_partial("X/02.wav", &env.wav("02.wav", 2, Some("X"), 2));
    assert!(matches!(env.freeze(), Err(UploadError::Conflict(s)) if s == "incomplete"));
    assert!(env.manifest().publish.is_none());
    assert!(env.manifest().publish_requested_at.is_none());
    // 外した origin は数えない
    env.edit(|m| m.origins[1].removed = true);
    assert_eq!(env.freeze().unwrap(), FreezeOutcome::Frozen);

    // 未展開の zip
    let env = Env::new();
    env.send("X/01.wav", &env.wav("01.wav", 1, Some("X"), 1));
    env.send("a.zip", b"PK-not-unpacked");
    assert!(matches!(env.freeze(), Err(UploadError::Conflict(s)) if s == "incomplete"));

    // 全件を外した
    let env = Env::new();
    env.send("X/01.wav", &env.wav("01.wav", 1, Some("X"), 1));
    let key = { env.freeze_probe_key() };
    env.edit(|m| m.excluded_groups.push(key));
    assert!(matches!(env.freeze(), Err(UploadError::Conflict(s)) if s == "empty"));
    assert!(env.manifest().publish.is_none());
}

impl Env {
    /// 唯一の件のキー（③ の表示から取る）
    fn freeze_probe_key(&self) -> String {
        let v = spindle::import::upload::build_view(
            &self.conn,
            &layout(),
            &self.inbox,
            &self.manifest(),
            &self.cfg,
            false,
        )
        .unwrap();
        assert_eq!(v.groups.len(), 1);
        v.groups[0].key.clone()
    }
}

#[test]
fn publishes_groups_to_upload_dirs() {
    let env = Env::new();
    env.two_groups();
    env.freeze().unwrap();
    let r = env.publish();
    assert!(r.finished);
    let mut placed = r.placed.clone();
    placed.sort();
    assert_eq!(placed, vec!["upload/Artist/X", "upload/_untitled/loose"]);
    let x = env.inbox_path().join("upload/Artist/X");
    assert_eq!(
        names_in(&x),
        vec!["01.wav", "02.wav", "cover.jpg", SIDECAR_NAME]
    );
    assert_eq!(
        names_in(&env.inbox_path().join("upload/_untitled/loose")),
        vec!["loose.wav", SIDECAR_NAME]
    );
    // 中身は送ったもの
    assert_eq!(std::fs::read(x.join("cover.jpg")).unwrap(), b"jpeg");
    // 全件が公開済み。discarded_at は公開では書かない
    let m = env.manifest();
    assert!(stages(&m)
        .iter()
        .all(|s| matches!(s, Stage::Published { .. })));
    assert!(m.discarded_at.is_none());
    assert!(m.inbox_enqueued_at.is_none());

    // 再実行は何もしない
    let tree = env.upload_tree();
    let r2 = env.publish();
    assert!(r2.finished);
    assert_eq!(env.upload_tree(), tree);
}

#[test]
fn suffixes_when_dir_exists() {
    let env = Env::new();
    env.send("X/01.wav", &env.wav("01.wav", 1, Some("X"), 1));
    let existing = env.inbox_path().join("upload/Artist/X");
    std::fs::create_dir_all(&existing).unwrap();
    std::fs::write(existing.join("keep.flac"), b"other").unwrap();
    env.freeze().unwrap();
    let r = env.publish();
    assert_eq!(r.placed, vec!["upload/Artist/X (2)"]);
    // 既存には触れない
    assert_eq!(names_in(&existing), vec!["keep.flac"]);
    assert_eq!(std::fs::read(existing.join("keep.flac")).unwrap(), b"other");
    assert_eq!(
        names_in(&env.inbox_path().join("upload/Artist/X (2)")),
        vec!["01.wav", SIDECAR_NAME]
    );
}

#[test]
fn untitled_uses_origin_label() {
    let env = Env::new();
    env.send("Live/01.wav", &env.wav("01.wav", 1, None, 1));
    env.send("Live/02.wav", &env.wav("02.wav", 2, None, 2));
    env.freeze().unwrap();
    let r = env.publish();
    assert_eq!(r.placed, vec!["upload/_untitled/Live"]);
    assert_eq!(
        names_in(&env.inbox_path().join("upload/_untitled/Live")),
        vec!["01.wav", "02.wav", SIDECAR_NAME]
    );
}

#[test]
fn companions_travel_flat() {
    let env = Env::new();
    env.send("Box/Disc 1/01.wav", &env.wav("01.wav", 1, Some("X"), 1));
    env.send("Box/Disc 2/01.wav", &env.wav("01.wav", 2, Some("X"), 2));
    env.send("Box/Scans/booklet.pdf", b"%PDF");
    env.freeze().unwrap();
    let plan = env.manifest().publish.unwrap();
    assert_eq!(plan.groups.len(), 1);
    let mut planned: Vec<String> = plan.groups[0]
        .members
        .iter()
        .map(|m| m.name.clone())
        .collect();
    env.publish();
    let dest = env.inbox_path().join("upload/Artist/X");
    // サブフォルダは作らず件の直下に平らに置く
    for e in std::fs::read_dir(&dest).unwrap() {
        assert!(e.unwrap().file_type().unwrap().is_file());
    }
    planned.push(SIDECAR_NAME.to_string());
    planned.sort();
    assert_eq!(names_in(&dest), planned);
    assert!(planned.contains(&"booklet.pdf".to_string()));
    assert_eq!(planned.len(), 4);
    assert_eq!(std::fs::read(dest.join("booklet.pdf")).unwrap(), b"%PDF");
}

#[test]
fn sidecar_has_upload_entry() {
    let env = Env::new();
    env.send("X/01.wav", &env.wav("01.wav", 1, Some("X"), 1));
    env.send("X/cover.jpg", b"jpeg");
    env.freeze().unwrap();
    let key = env.manifest().publish.unwrap().groups[0].key.clone();
    env.publish();
    let s = Sidecar::read(
        &env.inbox,
        &spindle::domain::relpath::RelPath::parse("upload/Artist/X").unwrap(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        s.upload,
        Some(UploadEntry {
            upload_id: env.id.as_str().to_string(),
            group_key: key,
            uploaded_at: NOW,
            origins: vec!["X/01.wav".to_string(), "X/cover.jpg".to_string()],
        })
    );
    // upload の無いサイドカーは従来どおりの形で書かれる（項が出ない）
    let plain = Sidecar::default().to_json();
    assert!(!String::from_utf8(plain).unwrap().contains("upload"));
}

#[test]
fn excluded_files_stay_in_staging() {
    let env = Env::new();
    env.two_groups();
    let v = spindle::import::upload::build_view(
        &env.conn,
        &layout(),
        &env.inbox,
        &env.manifest(),
        &env.cfg,
        false,
    )
    .unwrap();
    let x = v
        .groups
        .iter()
        .find(|g| g.album.as_deref() == Some("X"))
        .unwrap();
    let loose = v.groups.iter().find(|g| g.album.is_none()).unwrap();
    let dropped_track = x.tracks[1].staged.clone();
    let loose_key = loose.key.clone();
    let loose_staged = loose.tracks[0].staged.clone();
    env.edit(|m| {
        m.excluded_files.push(dropped_track.clone());
        m.excluded_groups.push(loose_key.clone());
    });
    env.freeze().unwrap();
    let r = env.publish();
    assert_eq!(r.placed, vec!["upload/Artist/X"]);
    assert_eq!(
        names_in(&env.inbox_path().join("upload/Artist/X")),
        vec!["01.wav", "cover.jpg", SIDECAR_NAME]
    );
    assert!(!env.inbox_path().join("upload/_untitled").exists());
    // 外したもの・取り込まないものはステージングに残る
    let staging = env.staging_path();
    assert!(staging.join(&dropped_track).is_file());
    assert!(staging.join(&loose_staged).is_file());
    let readme = env
        .manifest()
        .origins
        .iter()
        .find(|o| o.client_path == "X/readme.txt")
        .unwrap()
        .file_path()
        .unwrap();
    assert!(staging.join(readme.as_str()).is_file());
}

const ALL_STEPS: [PublishStep; 7] = [
    PublishStep::BeforeMove,
    PublishStep::AfterMove,
    PublishStep::AfterSidecar,
    PublishStep::AfterAssembledWrite,
    PublishStep::AfterDestWrite,
    PublishStep::AfterRename,
    PublishStep::AfterPublishedWrite,
];

#[test]
fn fault_at_each_step_then_rerun_publishes_exactly_once() {
    let files = Env::new().two_groups_files();
    let baseline = {
        let env = Env::new();
        env.send_all(&files);
        env.freeze().unwrap();
        assert!(env.publish().finished);
        env.upload_tree()
    };
    assert!(baseline.contains_key("Artist/X/01.wav"));

    for step in ALL_STEPS {
        let env = Env::new();
        env.send_all(&files);
        env.freeze().unwrap();
        let err = env.publish_with(&|s| s == step).unwrap_err();
        assert!(
            matches!(&err, UploadError::Io(e) if e.to_string() == "fault"),
            "{step:?}: {err:?}"
        );
        // 止まった時点では全件は終わっていない
        let m = env.manifest();
        assert!(
            !stages(&m)
                .iter()
                .all(|s| matches!(s, Stage::Published { .. })),
            "{step:?}"
        );
        let r = env.publish();
        assert!(r.finished, "{step:?}");
        assert_eq!(env.upload_tree(), baseline, "{step:?}");
        // 組み立て用のディレクトリは宛先へ移って残らない
        let publish_dir = env.staging_path().join("publish");
        assert!(names_in(&publish_dir).is_empty(), "{step:?}");
    }
}

#[test]
fn published_dir_consumed_by_inbox_counts_as_done() {
    let env = Env::new();
    env.send("X/01.wav", &env.wav("01.wav", 1, Some("X"), 1));
    env.freeze().unwrap();
    assert!(env
        .publish_with(&|s| s == PublishStep::AfterRename)
        .is_err());
    let dest = env.inbox_path().join("upload/Artist/X");
    assert!(dest.is_dir());
    assert!(matches!(
        &stages(&env.manifest())[0],
        Stage::Publishing { dest } if dest == "upload/Artist/X"
    ));
    // Inbox の配置が件を持っていき、空になった親も片付いた（テストの模擬）。公開済みにする前の
    // 宛先の親の fsync は、残っていないディレクトリを飛ばす
    std::fs::remove_dir_all(dest.parent().unwrap()).unwrap();
    let r = env.publish();
    assert!(r.finished);
    assert_eq!(
        stages(&env.manifest()),
        vec![Stage::Published {
            dest: "upload/Artist/X".into()
        }]
    );
    // 二重に置かない
    assert!(env.upload_tree().keys().all(|k| !k.ends_with(".wav")));
    assert!(!env.inbox_path().join("upload/Artist/X (2)").exists());
}

#[test]
fn dest_taken_while_assembled_picks_next() {
    // 組み立て済みで止まった後に宛先が埋まった
    let env = Env::new();
    env.send("X/01.wav", &env.wav("01.wav", 1, Some("X"), 1));
    env.freeze().unwrap();
    assert!(env
        .publish_with(&|s| s == PublishStep::AfterAssembledWrite)
        .is_err());
    assert_eq!(stages(&env.manifest()), vec![Stage::Assembled]);
    std::fs::create_dir_all(env.inbox_path().join("upload/Artist/X")).unwrap();
    let r = env.publish();
    assert_eq!(r.placed, vec!["upload/Artist/X (2)"]);
    assert!(names_in(&env.inbox_path().join("upload/Artist/X")).is_empty());

    // 宛先を書いた後（rename の前）に宛先が埋まった: 組み立て済みに戻して次の空きを選ぶ
    let env = Env::new();
    env.send("X/01.wav", &env.wav("01.wav", 1, Some("X"), 1));
    env.freeze().unwrap();
    assert!(env
        .publish_with(&|s| s == PublishStep::AfterDestWrite)
        .is_err());
    assert!(matches!(
        &stages(&env.manifest())[0],
        Stage::Publishing { dest } if dest == "upload/Artist/X"
    ));
    std::fs::create_dir_all(env.inbox_path().join("upload/Artist/X")).unwrap();
    let r = env.publish();
    assert_eq!(r.placed, vec!["upload/Artist/X (2)"]);
    assert!(names_in(&env.inbox_path().join("upload/Artist/X")).is_empty());
    assert_eq!(
        names_in(&env.inbox_path().join("upload/Artist/X (2)")),
        vec!["01.wav", SIDECAR_NAME]
    );
}

#[test]
fn discarded_or_cancelled_stops_before_each_group() {
    let env = Env::new();
    env.send("X/01.wav", &env.wav("01.wav", 1, Some("X"), 1));
    env.freeze().unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    let r = publish(&env.inbox, &env.locks, &env.id, NOW, &cancel, &|_| false).unwrap();
    assert!(!r.finished);
    assert_eq!(stages(&env.manifest()), vec![Stage::Planned]);

    env.edit(|m| m.discarded_at = Some(NOW));
    let r = env.publish();
    assert!(!r.finished);
    assert!(r.placed.is_empty());
    assert_eq!(stages(&env.manifest()), vec![Stage::Planned]);
    assert!(env.upload_tree().is_empty());
}

#[test]
fn published_dest_consumed_by_inbox_is_not_reused() {
    // 別の件が同じ宛先の名前になる（`?` は全角に置き換わる）。先の件を置き終え、Inbox の取り込みが
    // そのディレクトリを片付けた後に、後の件が宛先を選ぶ
    let env = Env::new();
    env.send("A/01.wav", &env.wav("01.wav", 1, Some("X?"), 1));
    env.send("B/01.wav", &env.wav("02.wav", 2, Some("X？"), 1));
    env.freeze().unwrap();
    let seen = std::cell::Cell::new(0);
    assert!(env
        .publish_with(&|s| {
            if s != PublishStep::AfterAssembledWrite {
                return false;
            }
            seen.set(seen.get() + 1);
            seen.get() == 2
        })
        .is_err());
    let st = stages(&env.manifest());
    assert_eq!(st.len(), 2);
    let Stage::Published { dest: first } = &st[0] else {
        panic!("先の件が置き終わっていない: {st:?}");
    };
    assert_eq!(first, "upload/Artist/X？");
    assert_eq!(st[1], Stage::Assembled);
    // Inbox の配置が先の件を持っていき、空になった親も片付いた（テストの模擬）
    std::fs::remove_dir_all(env.inbox_path().join("upload/Artist")).unwrap();
    let r = env.publish();
    assert!(r.finished);
    let mut placed = r.placed.clone();
    placed.sort();
    assert_eq!(placed, vec!["upload/Artist/X？", "upload/Artist/X？ (2)"]);
    assert_eq!(
        names_in(&env.inbox_path().join("upload/Artist/X？ (2)")),
        vec!["01.wav", SIDECAR_NAME]
    );
}
