//! CD の配置（`cd::place::place_disc`。SPEC §7.2、D-67、P2-8）。合成 PCM と確定したメタデータから
//! FLAC を Library に置き、DB に登録し、rip.log / disc.cue / disc.toc を書く。吸い出し（P2-5）は
//! まだ無いので `place_disc` を直接呼ぶ。`flac` が無ければ skip

#![cfg(target_os = "linux")]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use tokio_util::sync::CancellationToken;

use spindle::cd::metadata::{DiscMetadata, DiscTrackMetadata, MetadataSource};
use spindle::cd::place::{
    pcm_md5s, place_disc, plan_paths, PlaceEnv, PlaceError, PlaceInput, Placed,
};
use spindle::cd::riplog::{OffsetSource, RipReport, TrackCrcs, TrackRead};
use spindle::cd::toc::Toc;
use spindle::cd::verify::{MethodResult, Outcome, TrackVerdict};
use spindle::config::LayoutConfig;
use spindle::db::Db;
use spindle::fsroot::RootDir;
use spindle::import::scanner::{ScanKind, Scanner};
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

fn report(states_ok: bool) -> RipReport {
    let verdict = |i: u32| TrackVerdict {
        matched: states_ok,
        confidence: if states_ok { 4 } else { 0 },
        crc: i,
        crc_v2: None,
    };
    RipReport {
        drive: Some("TEST DRIVE".into()),
        device: "/dev/sr0".into(),
        read_offset: 0,
        offset_source: OffsetSource::Manual,
        started_at: 1_789_000_000,
        finished_at: 1_789_000_100,
        attempts: 1,
        encoder: "flac".into(),
        reads: vec![TrackRead::default(); 3],
        crcs: vec![TrackCrcs::default(); 3],
        ctdb: Some(MethodResult {
            outcome: if states_ok {
                Outcome::Verified
            } else {
                Outcome::NotFound
            },
            offset: 0,
            confidence: 0,
            tracks: (0..3).map(verdict).collect(),
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
    root: Arc<RootDir>,
    scanner: Scanner,
}

impl Lib {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("Library")).unwrap();
        std::fs::create_dir(dir.path().join("tmp")).unwrap();
        let db_path = dir.path().join("spindle.db");
        let db = Arc::new(Db::open(&db_path).unwrap());
        let root = Arc::new(RootDir::open(&dir.path().join("Library")).unwrap());
        let jobs = Jobs::new(db.clone());
        let scanner = Scanner::new(db.clone(), root.clone(), 2);
        Self {
            dir,
            db_path,
            db,
            jobs,
            root,
            scanner,
        }
    }

    fn env(&self) -> PlaceEnv {
        PlaceEnv {
            db: self.db.clone(),
            root: self.root.clone(),
            jobs: self.jobs.clone(),
            flac: PathBuf::from("flac"),
            compression: 5,
            tmp_dir: self.dir.path().join("tmp"),
            layout: LayoutConfig {
                multi_disc: "{category}/{albumartist}/{album}/{disc}-{track:02} {title}".into(),
                single_disc: "{category}/{albumartist}/{album}/{track:02} {title}".into(),
                unsorted: "_Unsorted/{albumartist}/{album}/{track:02} {title}".into(),
            },
            before_lock: None,
            before_register: None,
        }
    }

    fn conn(&self) -> Connection {
        Connection::open(&self.db_path).unwrap()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.path().join("Library").join(rel)
    }

    /// rip ジョブの行（running）。`library` の排他は running なジョブしか持てない
    fn running_job(&self) -> i64 {
        let c = self.conn();
        c.execute(
            "INSERT INTO jobs (type, state, payload, priority, attempts, max_attempts, created_at)
             VALUES ('rip', 'running', '{}', 0, 0, 1, 0)",
            [],
        )
        .unwrap();
        c.last_insert_rowid()
    }

    fn write_pcm(&self, bytes: &[u8]) -> PathBuf {
        let p = self.dir.path().join("tmp").join("disc.pcm");
        std::fs::write(&p, bytes).unwrap();
        p
    }

    async fn place(
        &self,
        m: &DiscMetadata,
        pcm: &Path,
        r: &RipReport,
    ) -> Result<Placed, PlaceError> {
        self.place_with(self.env(), &toc(), m, pcm, r).await
    }

    async fn place_with(
        &self,
        env: PlaceEnv,
        t: &Toc,
        m: &DiscMetadata,
        pcm: &Path,
        r: &RipReport,
    ) -> Result<Placed, PlaceError> {
        let job = self.running_job();
        place_disc(
            &env,
            PlaceInput {
                toc: t,
                metadata: m,
                pcm,
                report: r,
            },
            job,
            &CancellationToken::new(),
        )
        .await
    }

    async fn scan(&self) {
        self.scanner
            .run(
                ScanKind::Incremental,
                Arc::new(|_, _, _| {}),
                CancellationToken::new(),
            )
            .await
            .unwrap();
    }

    fn insert_category(&self, name: &str) -> i64 {
        let c = self.conn();
        c.execute("INSERT INTO categories (name) VALUES (?1)", [name])
            .unwrap();
        c.last_insert_rowid()
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
fn plan_uses_layout_templates_and_category() {
    let lib = Lib::new();
    let t = toc();
    let m = meta(None);
    let md5s = vec![[0u8; 16]; 3];
    let plan = plan_paths(&lib.conn(), &lib.env().layout, &t, &m, &md5s).unwrap();
    assert_eq!(plan.rel_dir.as_str(), "_Unsorted/Test Artist/Test Album");
    assert_eq!(
        plan.paths[0].as_str(),
        "_Unsorted/Test Artist/Test Album/01 Song 1.flac"
    );
    assert_eq!(plan.join_album, None);
    lib.insert_category("Rock");
    let m = meta(Some("Rock"));
    let plan = plan_paths(&lib.conn(), &lib.env().layout, &t, &m, &md5s).unwrap();
    assert_eq!(
        plan.paths[2].as_str(),
        "Rock/Test Artist/Test Album/03 Song 3.flac"
    );
    // 語彙の表記に揃える（大小文字違いは同じ語彙）
    let m = meta(Some("rock"));
    let plan = plan_paths(&lib.conn(), &lib.env().layout, &t, &m, &md5s).unwrap();
    assert!(plan.rel_dir.as_str().starts_with("Rock/"));
    // 語彙に無い category は _Unsorted（黙って新しいディレクトリを作らない）
    let m = meta(Some("Nope"));
    let plan = plan_paths(&lib.conn(), &lib.env().layout, &t, &m, &md5s).unwrap();
    assert!(plan.rel_dir.as_str().starts_with("_Unsorted/"));
    // 複数枚組は multi_disc テンプレート
    let mut m = meta(Some("Rock"));
    m.disc_count = 2;
    m.disc_no = 2;
    let plan = plan_paths(&lib.conn(), &lib.env().layout, &t, &m, &md5s).unwrap();
    assert_eq!(
        plan.paths[0].as_str(),
        "Rock/Test Artist/Test Album/2-01 Song 1.flac"
    );
    // 禁止文字は全角に
    let mut m = meta(None);
    m.album = "A/B: C?".into();
    let plan = plan_paths(&lib.conn(), &lib.env().layout, &t, &m, &md5s).unwrap();
    assert_eq!(plan.rel_dir.as_str(), "_Unsorted/Test Artist/A／B： C？");
    // 不正なメタデータは弾く
    let mut m = meta(None);
    m.album.clear();
    assert!(matches!(
        plan_paths(&lib.conn(), &lib.env().layout, &t, &m, &md5s),
        Err(PlaceError::Metadata(_))
    ));
}

// ---------------------------------------------------------------- 配置と登録

/// `(id, source_type, verification, audio_md5, album_id)`
fn track_row(lib: &Lib, rel: &str) -> (i64, String, String, Option<Vec<u8>>, i64) {
    lib.conn()
        .query_row(
            "SELECT id, source_type, verification, audio_md5, album_id FROM tracks
              WHERE rel_path = ?1 AND missing_since IS NULL",
            [rel],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )
        .unwrap()
}

fn count(lib: &Lib, sql: &str) -> i64 {
    lib.conn().query_row(sql, [], |r| r.get(0)).unwrap()
}

#[tokio::test]
async fn places_flac_with_tags_companions_rows_and_jobs() {
    require_flac!();
    let lib = Lib::new();
    lib.insert_category("Rock");
    let t = toc();
    let pcm = lib.write_pcm(&pcm_bytes(&t));
    let placed = lib
        .place(&meta(Some("Rock")), &pcm, &report(true))
        .await
        .unwrap();
    assert_eq!(placed.rel_dir.as_str(), "Rock/Test Artist/Test Album");
    assert_eq!(placed.track_ids.len(), 3);
    assert_eq!(placed.adopted, 0);
    assert_eq!(placed.reused_files, 0);

    // ファイルとタグ
    let p = lib.path("Rock/Test Artist/Test Album/02 Song 2.flac");
    assert!(p.exists());
    let af = spindle::domain::tags::read_audio_file(std::fs::File::open(&p).unwrap(), Some("flac"))
        .unwrap();
    assert_eq!(af.tags.first("TITLE"), Some("Song 2"));
    assert_eq!(af.tags.first("ARTIST"), Some("Test Artist"));
    assert_eq!(af.tags.first("ALBUM"), Some("Test Album"));
    assert_eq!(af.tags.first("TRACKNUMBER"), Some("2"));
    assert_eq!(af.tags.first("TRACKTOTAL"), Some("3"));
    assert_eq!(
        af.tags.first("MUSICBRAINZ_DISCID"),
        Some(t.musicbrainz_disc_id().as_str())
    );
    assert_eq!(af.sample_rate, Some(44100));
    assert_eq!(af.channels, Some(2));
    // STREAMINFO の MD5 は PCM の MD5
    let md5s = pcm_md5s(&pcm, &t.track_layout().unwrap()).unwrap();
    assert_eq!(
        spindle::media::fingerprint::flac_streaminfo_md5(std::fs::File::open(&p).unwrap()).unwrap(),
        Some(md5s[1])
    );
    // tmp が残っていない（作業領域にも Library にも）
    assert!(std::fs::read_dir(lib.dir.path().join("tmp"))
        .unwrap()
        .all(|e| e.unwrap().file_name() == "disc.pcm"));
    let names: Vec<String> = std::fs::read_dir(lib.path("Rock/Test Artist/Test Album"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().all(|n| !n.starts_with(".spindle-tmp-")),
        "{names:?}"
    );
    assert_eq!(names.len(), 6, "{names:?}");

    // 同梱ファイル
    let log = std::fs::read_to_string(lib.path("Rock/Test Artist/Test Album/rip.log")).unwrap();
    assert!(log.starts_with("spindle rip log v1\n"));
    assert!(log.contains("02 Song 2.flac"));
    assert!(log.contains("結果: verified_ctdb ×3\n"), "{log}");
    let cue = std::fs::read_to_string(lib.path("Rock/Test Artist/Test Album/disc.cue")).unwrap();
    assert!(cue.contains("FILE \"01 Song 1.flac\" WAVE"));
    assert!(lib.path("Rock/Test Artist/Test Album/disc.toc").exists());

    // DB
    let (id, src, ver, md5, album_id) =
        track_row(&lib, "Rock/Test Artist/Test Album/02 Song 2.flac");
    assert_eq!(src, "cd_rip");
    assert_eq!(ver, "verified_ctdb");
    assert_eq!(md5.as_deref(), Some(&md5s[1][..]));
    assert_eq!(album_id, placed.album_id);
    assert_eq!(placed.track_ids[1], id);
    let c = lib.conn();
    let (rel_dir, cat, aa): (String, Option<i64>, Option<String>) = c
        .query_row(
            "SELECT rel_dir, category_id, albumartist FROM albums WHERE id = ?1",
            [album_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    let (alb, discid, disc_count): (Option<String>, Option<String>, Option<i64>) = c
        .query_row(
            "SELECT album, discid, disc_count FROM albums WHERE id = ?1",
            [album_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(rel_dir, "Rock/Test Artist/Test Album");
    assert!(cat.is_some());
    assert_eq!(aa.as_deref(), Some("Test Artist"));
    assert_eq!(alb.as_deref(), Some("Test Album"));
    assert_eq!(discid.as_deref(), Some(t.musicbrainz_disc_id().as_str()));
    assert_eq!(disc_count, Some(1));
    let n_tags: i64 = c
        .query_row(
            "SELECT count(*) FROM track_tags WHERE track_id = ?1",
            [id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(n_tags >= 8, "{n_tags}");
    let (method, result, source, log_path): (String, String, String, Option<String>) = c
        .query_row(
            "SELECT method, result, source, log_path FROM album_verifications WHERE album_id = ?1",
            [album_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(
        (method.as_str(), result.as_str(), source.as_str()),
        ("ctdb", "verified", "rip")
    );
    assert_eq!(
        log_path.as_deref(),
        Some("Rock/Test Artist/Test Album/rip.log")
    );
    // AccurateRip は照会していないので行を作らない
    assert_eq!(count(&lib, "SELECT count(*) FROM album_verifications"), 1);
    assert_eq!(count(&lib, "SELECT count(*) FROM track_verifications"), 3);
    // 後続ジョブ: rg（album）と transcode ×3
    let jobs: Vec<String> = c
        .prepare("SELECT type FROM jobs WHERE state = 'queued' ORDER BY id")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(jobs.iter().filter(|t| *t == "rg").count(), 1);
    assert_eq!(jobs.iter().filter(|t| *t == "transcode").count(), 3);
    assert_eq!(placed.job_ids.len(), 4);
    // 排他は解放されている
    assert_eq!(count(&lib, "SELECT count(*) FROM job_mutexes"), 0);
    drop(c);

    // 次のスキャンで「変更なし」（重複登録しない、出自も残る）
    lib.scan().await;
    assert_eq!(count(&lib, "SELECT count(*) FROM tracks"), 3);
    assert_eq!(count(&lib, "SELECT count(*) FROM albums"), 1);
    assert_eq!(
        track_row(&lib, "Rock/Test Artist/Test Album/02 Song 2.flac").1,
        "cd_rip"
    );
}

#[tokio::test]
async fn unverified_disc_records_not_attempted_and_unsorted_path() {
    require_flac!();
    let lib = Lib::new();
    let t = toc();
    let pcm = lib.write_pcm(&pcm_bytes(&t));
    let placed = lib.place(&meta(None), &pcm, &report(false)).await.unwrap();
    assert_eq!(placed.rel_dir.as_str(), "_Unsorted/Test Artist/Test Album");
    let (_, src, ver, _, _) = track_row(&lib, "_Unsorted/Test Artist/Test Album/01 Song 1.flac");
    assert_eq!((src.as_str(), ver.as_str()), ("cd_rip", "not_attempted"));
    let result: String = lib
        .conn()
        .query_row(
            "SELECT result FROM album_verifications WHERE album_id = ?1 AND method = 'ctdb'",
            [placed.album_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(result, "not_found");
    let log =
        std::fs::read_to_string(lib.path("_Unsorted/Test Artist/Test Album/rip.log")).unwrap();
    assert!(log.contains("結果: not_attempted ×3\n"), "{log}");
}

#[tokio::test]
async fn second_disc_joins_existing_album_directory() {
    require_flac!();
    let lib = Lib::new();
    lib.insert_category("Rock");
    let t = toc();
    let pcm = lib.write_pcm(&pcm_bytes(&t));
    let mut d1 = meta(Some("Rock"));
    d1.disc_count = 2;
    d1.disc_no = 1;
    let first = lib.place(&d1, &pcm, &report(true)).await.unwrap();
    // 2 枚目は別の音声（MD5 が違う）
    let mut bytes = pcm_bytes(&t);
    for b in bytes.iter_mut().step_by(7) {
        *b = b.wrapping_add(1);
    }
    let pcm2 = lib.write_pcm(&bytes);
    let mut d2 = meta(Some("Rock"));
    d2.disc_count = 2;
    d2.disc_no = 2;
    let second = lib.place(&d2, &pcm2, &report(true)).await.unwrap();
    assert_eq!(second.album_id, first.album_id);
    assert_eq!(second.rel_dir, first.rel_dir);
    assert!(lib
        .path("Rock/Test Artist/Test Album/1-01 Song 1.flac")
        .exists());
    assert!(lib
        .path("Rock/Test Artist/Test Album/2-01 Song 1.flac")
        .exists());
    assert!(lib.path("Rock/Test Artist/Test Album/rip1.log").exists());
    assert!(lib.path("Rock/Test Artist/Test Album/rip2.log").exists());
    assert!(lib.path("Rock/Test Artist/Test Album/disc2.cue").exists());
    assert_eq!(count(&lib, "SELECT count(*) FROM albums"), 1);
    assert_eq!(count(&lib, "SELECT count(*) FROM tracks"), 6);
    // 手法 × ディスクで履歴が積まれる
    assert_eq!(
        count(
            &lib,
            "SELECT count(*) FROM album_verifications WHERE disc_no = 2"
        ),
        1
    );
    // 同じ disc_no をもう一度（別の音声）は合流せず、同じ TOC なので同一リリース扱いで衝突
    let mut bytes3 = pcm_bytes(&t);
    bytes3[10] ^= 0x7f;
    let pcm3 = lib.write_pcm(&bytes3);
    let err = lib.place(&d2, &pcm3, &report(true)).await.unwrap_err();
    assert!(matches!(err, PlaceError::Conflict(_)), "{err}");
    assert_eq!(count(&lib, "SELECT count(*) FROM tracks"), 6);
}

#[tokio::test]
async fn different_release_in_same_directory_is_demoted_with_year() {
    require_flac!();
    let lib = Lib::new();
    let t = toc();
    let pcm = lib.write_pcm(&pcm_bytes(&t));
    lib.place(&meta(None), &pcm, &report(true)).await.unwrap();
    // 同名・同アーティスト・1 枚組の別リリース（別 MB id）。PCM も別
    let mut bytes = pcm_bytes(&t);
    for b in bytes.iter_mut().step_by(5) {
        *b = b.wrapping_add(3);
    }
    let pcm2 = lib.write_pcm(&bytes);
    let mut m = meta(None);
    m.release_id = Some("mb-other".into());
    let placed = lib.place(&m, &pcm2, &report(true)).await.unwrap();
    assert_eq!(
        placed.rel_dir.as_str(),
        "_Unsorted/Test Artist/Test Album (2024)"
    );
    assert_eq!(count(&lib, "SELECT count(*) FROM albums"), 2);
}

#[tokio::test]
async fn same_disc_twice_is_idempotent_and_other_audio_is_a_conflict() {
    require_flac!();
    let lib = Lib::new();
    let t = toc();
    let pcm = lib.write_pcm(&pcm_bytes(&t));
    let first = lib.place(&meta(None), &pcm, &report(true)).await.unwrap();
    // 同じ MD5 の行が同じパスにある → 採用（冪等）で新しい行は増えない
    let again = lib.place(&meta(None), &pcm, &report(true)).await.unwrap();
    assert_eq!(again.adopted, 3);
    assert_eq!(again.reused_files, 6);
    assert_eq!(again.track_ids, first.track_ids);
    assert_eq!(count(&lib, "SELECT count(*) FROM tracks"), 3);
    // 別の音声で同じパス（同じ盤の吸い直し）は衝突。Library には何も増えない
    let mut bytes = pcm_bytes(&t);
    bytes[100] ^= 0x55;
    let pcm2 = lib.write_pcm(&bytes);
    let mut m = meta(None);
    m.date = None; // 年が無いので降格できない
    let err = lib.place(&m, &pcm2, &report(true)).await.unwrap_err();
    assert!(matches!(err, PlaceError::Conflict(_)), "{err}");
    let files: Vec<_> = std::fs::read_dir(lib.path("_Unsorted/Test Artist/Test Album"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(files.len(), 6, "{files:?}"); // 3 FLAC + cue + toc + log
    assert_eq!(count(&lib, "SELECT count(*) FROM tracks"), 3);
    assert_eq!(count(&lib, "SELECT count(*) FROM job_mutexes"), 0);
    // tmp のエンコード出力も残らない
    assert!(std::fs::read_dir(lib.dir.path().join("tmp"))
        .unwrap()
        .all(|e| e.unwrap().file_name() == "disc.pcm"));
}

#[tokio::test]
async fn rerun_after_crash_between_place_and_register_adopts_scanned_rows_and_reuses_files() {
    require_flac!();
    let lib = Lib::new();
    let t = toc();
    let pcm = lib.write_pcm(&pcm_bytes(&t));
    // 「配置の後、登録の前に落ちた」状態を作る: 一度配置してから行だけ消す
    let placed = lib.place(&meta(None), &pcm, &report(true)).await.unwrap();
    {
        let c = lib.conn();
        c.execute(
            "DELETE FROM album_verifications WHERE album_id = ?1",
            [placed.album_id],
        )
        .unwrap();
        c.execute("DELETE FROM tracks", []).unwrap();
        c.execute("DELETE FROM albums", []).unwrap();
        c.execute("DELETE FROM jobs WHERE state = 'queued'", [])
            .unwrap();
    }
    // 途中でスキャナが先に拾った
    lib.scan().await;
    let (scanned_id, src, ver, _, _) =
        track_row(&lib, "_Unsorted/Test Artist/Test Album/01 Song 1.flac");
    assert_eq!(src, "cd_rip"); // rip.log から復元
    assert_eq!(ver, "not_attempted");
    let again = lib.place(&meta(None), &pcm, &report(true)).await.unwrap();
    assert_eq!(again.adopted, 3);
    assert_eq!(again.reused_files, 6);
    assert_eq!(again.track_ids[0], scanned_id);
    let (_, _, ver, _, album_id) =
        track_row(&lib, "_Unsorted/Test Artist/Test Album/01 Song 1.flac");
    assert_eq!(ver, "verified_ctdb");
    assert_eq!(album_id, again.album_id);
    assert_eq!(count(&lib, "SELECT count(*) FROM tracks"), 3);
    assert_eq!(count(&lib, "SELECT count(*) FROM albums"), 1);
    assert_eq!(count(&lib, "SELECT count(*) FROM album_verifications"), 1);
}

#[tokio::test]
async fn busy_library_mutex_returns_busy_without_touching_files() {
    require_flac!();
    let lib = Lib::new();
    let t = toc();
    let pcm = lib.write_pcm(&pcm_bytes(&t));
    let other = lib.running_job();
    lib.conn()
        .execute(
            "INSERT INTO job_mutexes (name, job_id, acquired_at) VALUES ('library', ?1, 0)",
            [other],
        )
        .unwrap();
    let err = lib
        .place(&meta(None), &pcm, &report(true))
        .await
        .unwrap_err();
    assert!(matches!(err, PlaceError::Busy), "{err}");
    assert!(!lib.path("_Unsorted").exists());
    assert_eq!(count(&lib, "SELECT count(*) FROM tracks"), 0);
}

#[tokio::test]
async fn rejects_pcm_of_wrong_length_and_bad_metadata() {
    let lib = Lib::new();
    let t = toc();
    let mut bytes = pcm_bytes(&t);
    bytes.truncate(bytes.len() - 4);
    let pcm = lib.write_pcm(&bytes);
    let err = lib
        .place(&meta(None), &pcm, &report(true))
        .await
        .unwrap_err();
    assert!(matches!(err, PlaceError::PcmLength { .. }), "{err}");
    let pcm = lib.write_pcm(&pcm_bytes(&t));
    let mut m = meta(None);
    m.tracks[0].title.clear();
    let err = lib.place(&m, &pcm, &report(true)).await.unwrap_err();
    assert!(matches!(err, PlaceError::Metadata(_)), "{err}");
    assert!(!lib.path("_Unsorted").exists());
}

/// 4 トラックの別ディスク（別の TOC → 別の DiscID）
fn toc4() -> Toc {
    Toc::from_audio_sample_counts([
        700 * SECTOR as u64,
        650 * SECTOR as u64,
        800 * SECTOR as u64,
        500 * SECTOR as u64,
    ])
    .unwrap()
}

fn meta4(category: Option<&str>) -> DiscMetadata {
    let mut m = meta(category);
    m.tracks.push(DiscTrackMetadata {
        number: 4,
        title: "Song 4".into(),
        artist: String::new(),
        mb: None,
    });
    m
}

#[tokio::test]
async fn disc_with_other_release_id_does_not_join_and_is_demoted_consistently() {
    require_flac!();
    let lib = Lib::new();
    lib.insert_category("Rock");
    let t3 = toc();
    let pcm = lib.write_pcm(&pcm_bytes(&t3));
    let mut d1 = meta(Some("Rock"));
    d1.disc_count = 2;
    d1.release_id = Some("mb-A".into());
    let first = lib.place(&d1, &pcm, &report(true)).await.unwrap();
    // 同名・同アーティストの複数枚組だが別リリース（mb-B）の disc 2 → 合流せず ({year}) に降格し、
    // album もその降格先のもの（「ディレクトリ = album」）
    let t4 = toc4();
    let pcm4 = lib.write_pcm(&pcm_bytes(&t4));
    let mut d2 = meta4(Some("Rock"));
    d2.disc_count = 2;
    d2.disc_no = 2;
    d2.release_id = Some("mb-B".into());
    let second = lib
        .place_with(lib.env(), &t4, &d2, &pcm4, &report(true))
        .await
        .unwrap();
    assert_ne!(second.album_id, first.album_id);
    assert_eq!(
        second.rel_dir.as_str(),
        "Rock/Test Artist/Test Album (2024)"
    );
    let (_, _, _, _, album_id) =
        track_row(&lib, "Rock/Test Artist/Test Album (2024)/2-01 Song 1.flac");
    assert_eq!(album_id, second.album_id);
    let rel_dir: String = lib
        .conn()
        .query_row(
            "SELECT rel_dir FROM albums WHERE id = ?1",
            [second.album_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rel_dir, "Rock/Test Artist/Test Album (2024)");
}

#[tokio::test]
async fn multi_disc_input_does_not_join_a_single_disc_album() {
    require_flac!();
    let lib = Lib::new();
    lib.insert_category("Rock");
    let t3 = toc();
    let pcm = lib.write_pcm(&pcm_bytes(&t3));
    // 1 枚組として置いた album
    let first = lib
        .place(&meta(Some("Rock")), &pcm, &report(true))
        .await
        .unwrap();
    // 同名・同アーティストの「2 枚組の 2 枚目」（手入力、別 TOC）→ 1 枚組には合流しない。
    // 別リリースなので ({year}) に降格し、新しい album になる
    let t4 = toc4();
    let pcm4 = lib.write_pcm(&pcm_bytes(&t4));
    let mut d2 = meta4(Some("Rock"));
    d2.disc_count = 2;
    d2.disc_no = 2;
    let second = lib
        .place_with(lib.env(), &t4, &d2, &pcm4, &report(true))
        .await
        .unwrap();
    assert_ne!(second.album_id, first.album_id);
    assert_eq!(
        second.rel_dir.as_str(),
        "Rock/Test Artist/Test Album (2024)"
    );
    let disc_count: Option<i64> = lib
        .conn()
        .query_row(
            "SELECT disc_count FROM albums WHERE id = ?1",
            [first.album_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(disc_count, Some(1));
}

#[tokio::test]
async fn plan_is_redone_under_the_lock_when_the_library_moved_during_encoding() {
    require_flac!();
    let lib = Lib::new();
    lib.insert_category("Rock");
    let t3 = toc();
    let pcm = lib.write_pcm(&pcm_bytes(&t3));
    let mut d1 = meta(Some("Rock"));
    d1.disc_count = 2;
    let first = lib.place(&d1, &pcm, &report(true)).await.unwrap();
    // エンコードの間に album 全体が別ディレクトリへ動いた（rename ジョブの模擬: ファイルと DB）
    let db_path = lib.db_path.clone();
    let lib_dir = lib.dir.path().join("Library");
    let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        std::fs::create_dir_all(lib_dir.join("Rock/Moved")).unwrap();
        std::fs::rename(
            lib_dir.join("Rock/Test Artist/Test Album"),
            lib_dir.join("Rock/Moved/Test Album"),
        )
        .unwrap();
        let c = Connection::open(&db_path).unwrap();
        c.execute(
            "UPDATE albums SET rel_dir = 'Rock/Moved/Test Album', rel_dir_key = 'rock/moved/test album'",
            [],
        )
        .unwrap();
        c.execute(
            "UPDATE tracks SET rel_path = replace(rel_path, 'Rock/Test Artist/', 'Rock/Moved/'),
                               rel_path_key = replace(rel_path_key, 'rock/test artist/', 'rock/moved/')",
            [],
        )
        .unwrap();
    });
    let mut env = lib.env();
    env.before_lock = Some(hook);
    let t4 = toc4();
    let pcm4 = lib.write_pcm(&pcm_bytes(&t4));
    let mut d2 = meta4(Some("Rock"));
    d2.disc_count = 2;
    d2.disc_no = 2;
    let second = lib
        .place_with(env, &t4, &d2, &pcm4, &report(true))
        .await
        .unwrap();
    // 排他の中で取り直した計画: 元のディレクトリにはもう album が無いので新規 album になり、
    // 動いた先の album には所属しない
    assert_ne!(second.album_id, first.album_id);
    assert_eq!(second.rel_dir.as_str(), "Rock/Test Artist/Test Album");
    let (_, _, _, _, album_id) = track_row(&lib, "Rock/Test Artist/Test Album/2-01 Song 1.flac");
    assert_eq!(album_id, second.album_id);
    let rel_dir: String = lib
        .conn()
        .query_row(
            "SELECT rel_dir FROM albums WHERE id = ?1",
            [second.album_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rel_dir, "Rock/Test Artist/Test Album");
    assert!(lib.path("Rock/Moved/Test Album/1-01 Song 1.flac").exists());
}

/// 配置先のディレクトリにある自分（disc 2）以外のファイル名
fn files_of(lib: &Lib, rel_dir: &str) -> Vec<String> {
    match std::fs::read_dir(lib.path(rel_dir)) {
        Ok(rd) => {
            let mut v: Vec<String> = rd
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        }
        Err(_) => Vec::new(),
    }
}

#[tokio::test]
async fn join_album_moved_between_place_and_register_cleans_up_own_files() {
    require_flac!();
    let lib = Lib::new();
    lib.insert_category("Rock");
    let t3 = toc();
    let pcm = lib.write_pcm(&pcm_bytes(&t3));
    let mut d1 = meta(Some("Rock"));
    d1.disc_count = 2;
    let first = lib.place(&d1, &pcm, &report(true)).await.unwrap();
    // 配置の後・登録の前に rename ジョブが disc 1 の album を丸ごと動かした（トラックと同梱ファイル、DB）
    let db_path = lib.db_path.clone();
    let lib_dir = lib.dir.path().join("Library");
    let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let from = lib_dir.join("Rock/Test Artist/Test Album");
        let to = lib_dir.join("Rock/Moved/Test Album");
        std::fs::create_dir_all(&to).unwrap();
        for n in [
            "1-01 Song 1.flac",
            "1-02 Song 2.flac",
            "1-03 Song 3.flac",
            "disc1.cue",
            "disc1.toc",
            "rip1.log",
        ] {
            std::fs::rename(from.join(n), to.join(n)).unwrap();
        }
        let c = Connection::open(&db_path).unwrap();
        c.execute(
            "UPDATE albums SET rel_dir = 'Rock/Moved/Test Album', rel_dir_key = 'rock/moved/test album'",
            [],
        )
        .unwrap();
        c.execute(
            "UPDATE tracks SET rel_path = replace(rel_path, 'Rock/Test Artist/', 'Rock/Moved/'),
                               rel_path_key = replace(rel_path_key, 'rock/test artist/', 'rock/moved/')",
            [],
        )
        .unwrap();
    });
    let mut env = lib.env();
    env.before_register = Some(hook);
    let t4 = toc4();
    let pcm4 = lib.write_pcm(&pcm_bytes(&t4));
    let mut d2 = meta4(Some("Rock"));
    d2.disc_count = 2;
    d2.disc_no = 2;
    let err = lib
        .place_with(env, &t4, &d2, &pcm4, &report(true))
        .await
        .unwrap_err();
    assert!(matches!(err, PlaceError::Conflict(_)), "{err}");
    // 自分の成果物（FLAC 4 本と disc2.* / rip2.log）は残らず、空になった宛先も消える
    assert!(!lib.path("Rock/Test Artist/Test Album").exists());
    assert_eq!(
        files_of(&lib, "Rock/Moved/Test Album"),
        [
            "1-01 Song 1.flac",
            "1-02 Song 2.flac",
            "1-03 Song 3.flac",
            "disc1.cue",
            "disc1.toc",
            "rip1.log"
        ]
    );
    assert_eq!(count(&lib, "SELECT count(*) FROM tracks"), 3);
    assert_eq!(count(&lib, "SELECT count(*) FROM albums"), 1);
    assert_eq!(count(&lib, "SELECT count(*) FROM job_mutexes"), 0);
    let _ = first;
}

#[tokio::test]
async fn other_release_arriving_at_the_destination_before_register_is_a_conflict() {
    require_flac!();
    let lib = Lib::new();
    lib.insert_category("Rock");
    let t3 = toc();
    let pcm = lib.write_pcm(&pcm_bytes(&t3));
    // 配置の後・登録の前に、空だった宛先へ別リリース（別 MB id）の album が rename で入った
    // （ファイル名は衝突しない）
    let db_path = lib.db_path.clone();
    let lib_dir = lib.dir.path().join("Library");
    let hook: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
        let dir = lib_dir.join("Rock/Test Artist/Test Album");
        std::fs::write(dir.join("09 Other.flac"), b"not really flac").unwrap();
        let c = Connection::open(&db_path).unwrap();
        c.execute(
            "INSERT INTO albums (rel_dir, rel_dir_key, albumartist, album, mb_release_id)
             VALUES ('Rock/Test Artist/Test Album', 'rock/test artist/test album', 'Test Artist', 'Test Album', 'mb-other')",
            [],
        )
        .unwrap();
        let album_id = c.last_insert_rowid();
        c.execute(
            "INSERT INTO tracks (album_id, rel_path, rel_path_key, size, mtime_ns, ctime_ns, codec, lossless, seen_at)
             VALUES (?1, 'Rock/Test Artist/Test Album/09 Other.flac', 'rock/test artist/test album/09 other.flac', 15, 0, 0, 'flac', 1, 0)",
            [album_id],
        )
        .unwrap();
    });
    let mut env = lib.env();
    env.before_register = Some(hook);
    let err = lib
        .place_with(env, &t3, &meta(Some("Rock")), &pcm, &report(true))
        .await
        .unwrap_err();
    assert!(matches!(err, PlaceError::Conflict(_)), "{err}");
    // 別リリースの album には合流せず、自分の成果物だけ消えている
    assert_eq!(
        files_of(&lib, "Rock/Test Artist/Test Album"),
        ["09 Other.flac"]
    );
    assert_eq!(count(&lib, "SELECT count(*) FROM tracks"), 1);
    assert_eq!(count(&lib, "SELECT count(*) FROM albums"), 1);
    assert_eq!(count(&lib, "SELECT count(*) FROM album_verifications"), 0);
}
