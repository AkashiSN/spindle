//! CD の吸い出し結果を Inbox に置く（`cd::place::place_disc`。SPEC §7.2、D-67 追記、P2-5）。
//! 合成 PCM とメタデータから FLAC・同梱ファイル・サイドカーを Inbox の件として置く。承認して配置
//! すると Library に `cd_rip` と検証記録が入るところまで通す。`flac` が無ければ skip

#![cfg(target_os = "linux")]

mod common;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::cd::metadata::{DiscMetadata, DiscTrackMetadata, MetadataError, MetadataSource};
use spindle::cd::place::{
    inbox_dir, pcm_md5s, place_disc, PlaceEnv, PlaceError, PlaceHook, PlaceInput, Placed,
};
use spindle::cd::riplog::{OffsetSource, RipReport, TrackCrcs, TrackRead, RIP_LOG_SIGNATURE};
use spindle::cd::toc::Toc;
use spindle::cd::verify::{MethodResult, Outcome, TrackVerdict};
use spindle::config::LayoutConfig;
use spindle::db::inbox::{self as dbinbox, ItemState};
use spindle::db::Db;
use spindle::domain::relpath::RelPath;
use spindle::fsroot::RootDir;
use spindle::import::inbox::{place_item, propose, scan_inbox, PlaceItemEnv};
use spindle::import::sidecar::Sidecar;
use spindle::jobs::Jobs;

const SECTOR: usize = 588;

/// 3 トラック（10 / 8 / 12 秒）の TOC。サンプル数はセクタの倍数
fn toc() -> Toc {
    Toc::from_audio_sample_counts([
        750 * SECTOR as u64,
        600 * SECTOR as u64,
        900 * SECTOR as u64,
    ])
    .unwrap()
}

/// トラックごとに違う波形のインターリーブ i16（LE）
fn pcm_bytes(t: &Toc) -> Vec<u8> {
    let layout = t.track_layout().unwrap();
    let mut out = Vec::new();
    for (i, &n) in layout.lengths().iter().enumerate() {
        for k in 0..n {
            let l = (((k as f64) * 0.05 * (i as f64 + 1.0)).sin() * 12000.0) as i16;
            let r = (((k as f64) * 0.031).cos() * 9000.0) as i16;
            out.extend_from_slice(&l.to_le_bytes());
            out.extend_from_slice(&r.to_le_bytes());
        }
    }
    out
}

fn meta(category: Option<&str>) -> DiscMetadata {
    DiscMetadata {
        source: MetadataSource::Manual,
        release_id: None,
        release_group_id: None,
        album: "Test Album".into(),
        album_artist: "Test Artist".into(),
        date: Some("2024".into()),
        label: None,
        catalog_number: None,
        barcode: None,
        disc_no: 1,
        disc_count: 1,
        category: category.map(str::to_owned),
        tracks: (1..=3u8)
            .map(|n| DiscTrackMetadata {
                number: n,
                title: format!("Song {n}"),
                artist: String::new(),
                mb: None,
            })
            .collect(),
    }
}

/// 候補ゼロ件の盤（名前もタイトルも空）
fn empty_meta() -> DiscMetadata {
    let mut m = meta(None);
    m.album.clear();
    m.album_artist.clear();
    m.date = None;
    for t in &mut m.tracks {
        t.title.clear();
    }
    m
}

/// CTDB は 1 本目と 3 本目が一致、2 本目が不一致
fn report() -> RipReport {
    let matched = [true, false, true];
    RipReport {
        drive: Some("TEST DRIVE".into()),
        device: "/dev/sr0".into(),
        read_offset: 6,
        offset_source: OffsetSource::Learned,
        started_at: 1_789_000_000,
        finished_at: 1_789_000_100,
        attempts: 1,
        encoder: "flac".into(),
        reads: vec![TrackRead::default(); 3],
        crcs: (0..3u32)
            .map(|i| TrackCrcs {
                ar_v1: 1 + i,
                ar_v2: 11 + i,
                ctdb: 100 + i,
            })
            .collect(),
        ctdb: Some(MethodResult {
            outcome: Outcome::Mismatch,
            offset: 0,
            confidence: 0,
            tracks: matched
                .iter()
                .enumerate()
                .map(|(i, &m)| TrackVerdict {
                    matched: m,
                    confidence: if m { 3 } else { 0 },
                    crc: 100 + i as u32,
                    crc_v2: None,
                })
                .collect(),
        }),
        accuraterip: None,
        repaired_words: None,
    }
}

struct Lib {
    dir: tempfile::TempDir,
    db_path: PathBuf,
    db: Arc<Db>,
    jobs: Arc<Jobs>,
    inbox: Arc<RootDir>,
    library: Arc<RootDir>,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for d in ["Library", "Inbox", "tmp"] {
            std::fs::create_dir(dir.path().join(d)).unwrap();
        }
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        common::enable_opus_variant(&db_path, 128);
        let inbox = Arc::new(RootDir::open(&dir.path().join("Inbox")).unwrap());
        let library = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
        let jobs = Jobs::new(db.clone());
        Self {
            dir,
            db_path,
            db,
            jobs,
            inbox,
            library,
        }
    }

    fn env(&self) -> PlaceEnv {
        self.env_with(None)
    }

    fn env_with(&self, before_publish: Option<PlaceHook>) -> PlaceEnv {
        PlaceEnv {
            inbox: self.inbox.clone(),
            jobs: self.jobs.clone(),
            flac: PathBuf::from("flac"),
            compression: 5,
            tmp_dir: self.dir.path().join("tmp"),
            before_publish,
        }
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    fn inbox_path(&self, rel: &str) -> PathBuf {
        self.dir.path().join("Inbox").join(rel)
    }

    fn write_pcm(&self, bytes: &[u8]) -> PathBuf {
        let p = self.dir.path().join("tmp").join("disc.pcm");
        std::fs::write(&p, bytes).unwrap();
        p
    }

    async fn place_with(
        &self,
        env: PlaceEnv,
        m: &DiscMetadata,
        pcm: &Path,
    ) -> Result<Placed, PlaceError> {
        place_disc(
            &env,
            PlaceInput {
                toc: &toc(),
                metadata: m,
                pcm,
                report: &report(),
            },
            &CancellationToken::new(),
        )
        .await
    }

    async fn place(&self, m: &DiscMetadata) -> Result<Placed, PlaceError> {
        let pcm = self.write_pcm(&pcm_bytes(&toc()));
        self.place_with(self.env(), m, &pcm).await
    }

    fn count(&self, sql: &str) -> i64 {
        self.conn().query_row(sql, [], |r| r.get(0)).unwrap()
    }

    fn inbox_entries(&self) -> Vec<String> {
        let mut out: Vec<String> = std::fs::read_dir(self.dir.path().join("Inbox"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        out.sort();
        out
    }
}

macro_rules! require_flac {
    () => {
        if std::process::Command::new("flac")
            .arg("--version")
            .output()
            .is_err()
        {
            eprintln!("flac が無いので skip");
            return;
        }
    };
}

fn tags_of(p: &Path) -> spindle::domain::tags::AudioFile {
    spindle::domain::tags::read_audio_file(std::fs::File::open(p).unwrap(), Some("flac")).unwrap()
}

#[test]
fn pcm_md5_per_track_matches_streaminfo_style() {
    let t = toc();
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("d.pcm");
    let bytes = pcm_bytes(&t);
    std::fs::write(&p, &bytes).unwrap();
    let md5s = pcm_md5s(&p, &t.track_layout().unwrap()).unwrap();
    assert_eq!(md5s.len(), 3);
    // トラック 1 のバイト列をそのまま md5 したものと一致（STREAMINFO は LE インターリーブの MD5）
    use md5::Digest as _;
    let n1 = 750 * SECTOR * 4;
    assert_eq!(md5s[0], <[u8; 16]>::from(md5::Md5::digest(&bytes[..n1])));
    assert_ne!(md5s[0], md5s[1]);
    // 長さが合わなければエラー
    std::fs::write(&p, &bytes[..bytes.len() - 4]).unwrap();
    assert!(pcm_md5s(&p, &t.track_layout().unwrap()).is_err());
}

#[test]
fn inbox_dir_is_named_from_names_and_disc_id() {
    let t = toc();
    let id = t.musicbrainz_disc_id();
    assert_eq!(
        inbox_dir(&t, &meta(None)).unwrap().as_str(),
        format!("CD/Test Artist - Test Album [{id}]")
    );
    // 名前が無ければ DiscID だけ（DiscID は `.` で始まり得るので括弧で包む。この TOC の DiscID が
    // まさにそう）
    assert!(id.starts_with('.'), "{id}");
    assert_eq!(
        inbox_dir(&t, &empty_meta()).unwrap().as_str(),
        format!("CD/[{id}]")
    );
    // 片方だけ
    let mut m = empty_meta();
    m.album = "Only".into();
    assert_eq!(
        inbox_dir(&t, &m).unwrap().as_str(),
        format!("CD/Only [{id}]")
    );
    // 区切り文字は置き換え、先頭の `.` は落とす（隠しディレクトリにしない）
    let mut m = meta(None);
    m.album_artist = "..A/B".into();
    m.album = "C".into();
    let dir = inbox_dir(&t, &m).unwrap();
    let name = dir.file_name();
    assert!(!name.starts_with('.'), "{name}");
    assert!(!name[..name.len() - id.len()].contains('/'));
    // 長すぎる名前は切り詰める（255 バイトに収まる）
    let mut m = meta(None);
    m.album = "あ".repeat(200);
    let dir = inbox_dir(&t, &m).unwrap();
    assert!(dir.file_name().len() <= 255, "{}", dir.file_name().len());
    assert!(dir.file_name().ends_with(&format!("[{id}]")));
}

#[tokio::test]
async fn places_disc_in_inbox_with_tags_companions_and_sidecar() {
    require_flac!();
    let lib = Lib::new();
    let placed = lib.place(&meta(Some("Rock"))).await.unwrap();
    let id = toc().musicbrainz_disc_id();
    let dir = format!("CD/Test Artist - Test Album [{id}]");
    assert_eq!(placed.rel_dir.as_str(), dir);
    assert_eq!(placed.files, ["01.flac", "02.flac", "03.flac"]);
    assert!(!placed.reused);

    // FLAC とタグ（TRACKTOTAL / MUSICBRAINZ_DISCID は TOC から）
    let af = tags_of(&lib.inbox_path(&format!("{dir}/02.flac")));
    assert_eq!(af.tags.first("TITLE"), Some("Song 2"));
    assert_eq!(af.tags.first("ALBUM"), Some("Test Album"));
    assert_eq!(af.tags.first("ALBUMARTIST"), Some("Test Artist"));
    assert_eq!(af.tags.first("TRACKNUMBER"), Some("2"));
    assert_eq!(af.tags.first("TRACKTOTAL"), Some("3"));
    assert_eq!(af.tags.first("MUSICBRAINZ_DISCID"), Some(id.as_str()));
    // category はタグに書かない（パス専用。SPEC §5）
    assert_eq!(af.tags.first("GENRE"), None);

    // 同梱ファイル
    let log = std::fs::read_to_string(lib.inbox_path(&format!("{dir}/rip.log"))).unwrap();
    assert_eq!(log.lines().next(), Some(RIP_LOG_SIGNATURE));
    assert!(lib.inbox_path(&format!("{dir}/disc.cue")).exists());
    assert!(lib.inbox_path(&format!("{dir}/disc.toc")).exists());

    // サイドカー: category と、名前で結びつけられる吸い出しの記録
    let s = Sidecar::read(&lib.inbox, &RelPath::parse(&dir).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(s.category.as_deref(), Some("Rock"));
    let rip = s.rip.unwrap();
    assert_eq!(rip.files, placed.files);
    assert_eq!(rip.log, "rip.log");
    assert_eq!(rip.report, report());
    assert_eq!(rip.toc, toc().ctdb_toc());
    rip.check().unwrap();

    // 組み立て用の隠しディレクトリは残らない。inbox ジョブを投入した
    assert_eq!(lib.inbox_entries(), ["CD"]);
    assert_eq!(
        lib.count(&format!(
            "SELECT count(*) FROM jobs WHERE id = {} AND type = 'inbox'",
            placed.inbox_job
        )),
        1
    );
}

/// 受け入れ: 候補ゼロ件・アルバム名もアーティストも空のまま Inbox まで完走し、承認画面で名前を
/// 入れれば Library に `cd_rip` と検証記録付きで入る。承認でトラック番号を入れ替えても、検証記録は
/// 名前で結びついた行に付く
#[tokio::test]
async fn nameless_disc_completes_to_inbox_and_through_approval() {
    require_flac!();
    let lib = Lib::new();
    let placed = lib.place(&empty_meta()).await.unwrap();
    let dir = placed.rel_dir.as_str().to_owned();
    assert_eq!(dir, format!("CD/[{}]", toc().musicbrainz_disc_id()));
    let af = tags_of(&lib.inbox_path(&format!("{dir}/01.flac")));
    assert_eq!(af.tags.first("TITLE"), Some("Track 01"));
    assert_eq!(af.tags.first("ALBUM"), None);
    // 記録には受け取った内容（空の名前）を残す
    let s = Sidecar::read(&lib.inbox, &placed.rel_dir).unwrap().unwrap();
    assert_eq!(s.category, None);
    assert_eq!(s.rip.as_ref().unwrap().metadata, empty_meta());

    // 走査で件になる。提案は album gain on、名前が無いので警告が出る（承認で直す）
    scan_inbox(&lib.db, &lib.inbox, 1000).await.unwrap();
    let key = spindle::domain::relpath::canonical_key(&dir);
    let item = dbinbox::find_by_dir_key(&lib.conn(), &key)
        .unwrap()
        .unwrap();
    let files = dbinbox::files(&lib.conn(), item.id).unwrap();
    assert_eq!(files.len(), 3);
    let layout = LayoutConfig {
        multi_disc: "{category}/{albumartist}/{album}/{disc}-{track:02} {title}".into(),
        single_disc: "{category}/{albumartist}/{album}/{track:02} {title}".into(),
        unsorted: "_Unsorted/{albumartist}/{album}/{track:02} {title}".into(),
    };
    let proposed = propose(&lib.conn(), &lib.inbox, &layout, &item, &files, &[], &[]).unwrap();
    assert!(proposed.draft.album_gain);
    assert_eq!(proposed.draft.album, "");
    assert_eq!(proposed.draft.tracks[0].title, "Track 01");
    assert!(proposed
        .warnings
        .iter()
        .any(|w| w.contains("アルバム名が空")));

    // 承認画面で名前を入れ、01 と 03 の番号を入れ替える
    let mut draft = proposed.draft.clone();
    draft.albumartist = "Artist".into();
    draft.album = "Named".into();
    for t in &mut draft.tracks {
        if t.rel_path.ends_with("01.flac") {
            t.track_no = 3;
            t.title = "Uno".into();
        } else if t.rel_path.ends_with("03.flac") {
            t.track_no = 1;
            t.title = "Tres".into();
        }
    }
    {
        let c = lib.conn();
        dbinbox::set_draft(&c, item.id, &serde_json::to_value(&draft).unwrap()).unwrap();
        dbinbox::set_state(&c, item.id, ItemState::Approved, None, 1001).unwrap();
    }
    let item = dbinbox::get(&lib.conn(), item.id).unwrap().unwrap();
    let env = PlaceItemEnv {
        db: lib.db.clone(),
        library: lib.library.clone(),
        inbox: lib.inbox.clone(),
        jobs: lib.jobs.clone(),
        layout,
        editor: None,
        wav_to_flac: false,
        before_place: None,
        artwork: None,
        before_artwork: None,
    };
    let out = place_item(&env, &item, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(out.rel_dir.as_str(), "_Unsorted/Artist/Named");

    let c = lib.conn();
    let row = |rel: &str| -> (String, String, i64) {
        c.query_row(
            "SELECT t.source_type, t.verification, tv.ctdb_crc
               FROM tracks t JOIN track_verifications tv ON tv.track_id = t.id
              WHERE t.rel_path = ?1",
            [rel],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    assert_eq!(
        row("_Unsorted/Artist/Named/03 Uno.flac"),
        ("cd_rip".into(), "verified_ctdb".into(), 100)
    );
    assert_eq!(
        row("_Unsorted/Artist/Named/02 Track 02.flac"),
        ("cd_rip".into(), "mismatch".into(), 101)
    );
    assert_eq!(
        row("_Unsorted/Artist/Named/01 Tres.flac"),
        ("cd_rip".into(), "verified_ctdb".into(), 102)
    );
    let log_path: String = c
        .query_row("SELECT log_path FROM album_verifications", [], |r| r.get(0))
        .unwrap();
    assert_eq!(log_path, "_Unsorted/Artist/Named/rip.log");
    assert!(lib.dir.path().join("Library").join(&log_path).exists());
    // Inbox の件は消費された
    assert!(!lib.inbox_path(&dir).exists());
}

/// 公開の後に落ちた再実行は、公開先の自分の成果物をそのまま使う
#[tokio::test]
async fn rerun_after_publish_reuses_the_published_item() {
    require_flac!();
    let lib = Lib::new();
    let first = lib.place(&meta(None)).await.unwrap();
    let p = lib.inbox_path(&format!("{}/01.flac", first.rel_dir));
    let ino = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&p).unwrap());
    let second = lib.place(&meta(None)).await.unwrap();
    assert!(second.reused);
    assert_eq!(second.rel_dir, first.rel_dir);
    assert_eq!(
        std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&p).unwrap()),
        ino
    );
    assert_eq!(lib.inbox_entries(), ["CD"]);
}

/// 前の実行の組み立ての残骸（隠しディレクトリ）は消して作り直す。走査は隠しディレクトリを見ない
#[tokio::test]
async fn stale_staging_is_rebuilt_and_never_seen_by_the_scan() {
    require_flac!();
    let lib = Lib::new();
    let staging = format!(".spindle-rip-{}", toc().musicbrainz_disc_id());
    std::fs::create_dir(lib.inbox_path(&staging)).unwrap();
    std::fs::write(lib.inbox_path(&format!("{staging}/01.flac")), b"half").unwrap();
    std::fs::write(lib.inbox_path(&format!("{staging}/.spindle-tmp-x")), b"x").unwrap();
    let out = scan_inbox(&lib.db, &lib.inbox, 1000).await.unwrap();
    assert_eq!(out.items_seen, 0);

    let placed = lib.place(&meta(None)).await.unwrap();
    assert!(!placed.reused);
    assert_eq!(lib.inbox_entries(), ["CD"]);
    let af = tags_of(&lib.inbox_path(&format!("{}/01.flac", placed.rel_dir)));
    assert_eq!(af.tags.first("TITLE"), Some("Song 1"));
}

/// 公開先に別の内容の件があれば衝突。組み立てたものは残さず、既存の件にも触れない
#[tokio::test]
async fn other_item_at_the_destination_is_a_conflict() {
    require_flac!();
    let lib = Lib::new();
    let dir = inbox_dir(&toc(), &meta(None)).unwrap();
    // 事前にある（確認の段階で弾く）
    std::fs::create_dir_all(lib.inbox_path(dir.as_str())).unwrap();
    std::fs::write(lib.inbox_path(&format!("{dir}/01.flac")), b"other").unwrap();
    assert!(matches!(
        lib.place(&meta(None)).await,
        Err(PlaceError::Conflict(_))
    ));
    assert_eq!(
        std::fs::read(lib.inbox_path(&format!("{dir}/01.flac"))).unwrap(),
        b"other"
    );
    std::fs::remove_dir_all(lib.inbox_path("CD")).unwrap();

    // 組み立ての間に現れた（公開の rename で弾く）
    let target = lib.inbox_path(dir.as_str());
    let hook: PlaceHook = Arc::new(move || {
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("x.flac"), b"other").unwrap();
    });
    let pcm = lib.write_pcm(&pcm_bytes(&toc()));
    assert!(matches!(
        lib.place_with(lib.env_with(Some(hook)), &meta(None), &pcm)
            .await,
        Err(PlaceError::Conflict(_))
    ));
    assert_eq!(lib.inbox_entries(), ["CD"]);
    assert!(lib.inbox_path(&format!("{dir}/x.flac")).exists());
    assert!(!lib.inbox_path(&format!("{dir}/01.flac")).exists());
}

#[tokio::test]
async fn rejects_pcm_of_wrong_length_and_bad_metadata() {
    let lib = Lib::new();
    let pcm = lib.write_pcm(&pcm_bytes(&toc())[..100]);
    assert!(matches!(
        lib.place_with(lib.env(), &meta(None), &pcm).await,
        Err(PlaceError::PcmLength { .. })
    ));
    let mut m = meta(None);
    m.tracks.pop();
    let pcm = lib.write_pcm(&pcm_bytes(&toc()));
    assert!(matches!(
        lib.place_with(lib.env(), &m, &pcm).await,
        Err(PlaceError::Metadata(MetadataError::TrackCount { .. }))
    ));
    assert!(lib.inbox_entries().is_empty());
}
