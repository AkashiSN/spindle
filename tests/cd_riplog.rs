//! rip.log / disc.cue / disc.toc の描画と `RipReport`（`cd::riplog`。SPEC §7.2、D-67、P2-8）。
//! 同梱ファイルの名前（1 枚 / 複数枚組）、スキャナの判定に使う署名、cue（EAC 流の複数ファイル）、
//! cdrdao 構文の toc、人間可読の log

use spindle::cd::metadata::{DiscMetadata, DiscTrackMetadata, MetadataSource, TrackMbIds};
use spindle::cd::riplog::{
    companion_names, is_companion_name, is_rip_log_name, msf, render_cue, render_log, render_toc,
    CompanionNames, OffsetSource, RipReport, TrackCrcs, TrackRead, RIP_LOG_SIGNATURE,
};
use spindle::cd::toc::Toc;
use spindle::cd::verify::{MethodResult, Outcome, TrackVerdict};

const NEVERMIND: &str =
    "0:22593:41700:58133:71920:91198:104468:115188:131988:143758:159678:174415:191880";
const HYBRID_THEORY_JP: &str = "0:13915:25592:40835:55855:71530:85325:99560:115782:129627:144212:156000:170495:190165:-218477:241060";

fn toc() -> Toc {
    Toc::parse(NEVERMIND).unwrap()
}

fn meta_n(n: usize) -> DiscMetadata {
    DiscMetadata {
        source: MetadataSource::Manual,
        release_id: None,
        release_group_id: None,
        album: "Nevermind".into(),
        album_artist: "Nirvana".into(),
        date: Some("1991-09-24".into()),
        label: Some("DGC".into()),
        catalog_number: Some("DGCD-24425".into()),
        barcode: Some("720642442524".into()),
        disc_no: 1,
        disc_count: 1,
        category: None,
        tracks: (1..=n as u8)
            .map(|k| DiscTrackMetadata {
                number: k,
                title: format!("T{k}"),
                artist: String::new(),
                mb: None,
            })
            .collect(),
    }
}

fn meta() -> DiscMetadata {
    meta_n(12)
}

fn names(n: usize) -> Vec<String> {
    (1..=n).map(|i| format!("{i:02} T{i}.flac")).collect()
}

fn report(n: usize) -> RipReport {
    RipReport {
        drive: Some("ASUS BW-16D1HT 3.10".into()),
        device: "/dev/sr0".into(),
        read_offset: 6,
        offset_source: OffsetSource::Learned,
        started_at: 1_789_000_000,
        finished_at: 1_789_000_600,
        attempts: 1,
        attempt_slips: Vec::new(),
        encoder: "flac 1.5.0 -8 --verify".into(),
        reads: vec![TrackRead::default(); n],
        crcs: (0..n)
            .map(|i| TrackCrcs {
                ar_v1: 0x1000 + i as u32,
                ar_v2: 0x2000 + i as u32,
                ctdb: 0x3000 + i as u32,
            })
            .collect(),
        ctdb: Some(MethodResult {
            outcome: Outcome::Verified,
            offset: 0,
            confidence: 5,
            tracks: (0..n)
                .map(|i| TrackVerdict {
                    matched: true,
                    confidence: 5,
                    crc: 0x3000 + i as u32,
                    crc_v2: None,
                })
                .collect(),
        }),
        accuraterip: Some(MethodResult {
            outcome: Outcome::Mismatch,
            offset: 0,
            confidence: 0,
            tracks: (0..n)
                .map(|i| TrackVerdict {
                    matched: i != 3,
                    confidence: if i != 3 { 12 } else { 0 },
                    crc: 0x1000 + i as u32,
                    crc_v2: Some(0x2000 + i as u32),
                })
                .collect(),
        }),
        repaired_words: Some(3),
    }
}

#[test]
fn companion_names_single_and_multi() {
    assert_eq!(
        companion_names(1, 1),
        CompanionNames {
            cue: "disc.cue".into(),
            toc: "disc.toc".into(),
            log: "rip.log".into()
        }
    );
    assert_eq!(
        companion_names(2, 3),
        CompanionNames {
            cue: "disc2.cue".into(),
            toc: "disc2.toc".into(),
            log: "rip2.log".into()
        }
    );
    assert_eq!(companion_names(1, 2).log, "rip1.log");
    for n in ["rip.log", "RIP.LOG", "rip2.log", "rip12.log"] {
        assert!(is_rip_log_name(n), "{n}");
    }
    for n in [
        "rip.txt",
        "riplog",
        "rip-2.log",
        "verify.log",
        "ripx.log",
        "rip.log.bak",
    ] {
        assert!(!is_rip_log_name(n), "{n}");
    }
    for n in [
        "cover.jpg",
        "Folder.png",
        "disc.cue",
        "disc2.toc",
        "DISC.CUE",
        "rip.log",
    ] {
        assert!(is_companion_name(n), "{n}");
    }
    for n in [
        "01 T1.flac",
        "scan.jpg",
        "disc.txt",
        "notes.cue",
        "disc-2.cue",
    ] {
        assert!(!is_companion_name(n), "{n}");
    }
    assert_eq!(msf(0), "00:00:00");
    assert_eq!(msf(22593), "05:01:18"); // 5*60*75 + 1*75 + 18
    assert_eq!(msf(191880), "42:38:30");
}

#[test]
fn cue_is_multi_file_with_header_and_index_01() {
    let cue = render_cue(&toc(), &meta(), &names(12));
    let lines: Vec<&str> = cue.lines().collect();
    assert_eq!(lines[0], "REM DISCID y6Br7t4P.bldLe_6Im2d9Z42IU4-");
    assert!(cue.contains("REM DATE 1991-09-24\n"));
    // 12 桁の UPC は 13 桁に 0 埋め
    assert!(cue.contains("CATALOG 0720642442524\n"));
    assert!(cue.contains("PERFORMER \"Nirvana\"\nTITLE \"Nevermind\"\n"));
    assert!(cue.contains(
        "FILE \"01 T1.flac\" WAVE\n  TRACK 01 AUDIO\n    TITLE \"T1\"\n    PERFORMER \"Nirvana\"\n    INDEX 01 00:00:00\n"
    ), "{cue}");
    assert!(cue.contains("FILE \"12 T12.flac\" WAVE\n  TRACK 12 AUDIO\n"));
    assert!(!cue.contains("INDEX 00"));
    assert!(!cue.contains("REM DATA TRACK"));
    // 引用符は落とす（cue には規格が無い）。日付・バーコードが無ければ行ごと出ない
    let mut m = meta();
    m.tracks[0].title = "Say \"Hi\"".into();
    m.date = None;
    m.barcode = Some("abc".into());
    let cue = render_cue(&toc(), &m, &names(12));
    assert!(cue.contains("TITLE \"Say Hi\""));
    assert!(!cue.contains("REM DATE"));
    assert!(!cue.contains("CATALOG"));
}

#[test]
fn cue_and_toc_note_data_track_and_isrc() {
    let t = Toc::parse(HYBRID_THEORY_JP).unwrap();
    let n = t.audio_tracks().count();
    assert_eq!(n, 14);
    let mut m = meta_n(n);
    m.tracks[0].mb = Some(TrackMbIds {
        recording_id: "r".into(),
        track_id: "t".into(),
        isrcs: vec!["USWB10100001".into(), "JPXX00000002".into()],
    });
    let cue = render_cue(&t, &m, &names(n));
    assert!(cue.contains("  TRACK 01 AUDIO\n    TITLE \"T1\"\n    PERFORMER \"Nirvana\"\n    ISRC USWB10100001\n    INDEX 01 00:00:00\n"), "{cue}");
    assert!(cue.contains("REM DATA TRACK 15 LBA 218477\n"));
    assert!(!cue.contains("TRACK 15 AUDIO"));
    let toc_text = render_toc(&t, &m, &names(n));
    assert!(toc_text.starts_with("CD_DA\n"));
    assert!(toc_text.contains("// data track 15 at LBA 218477 (not ripped)\n"));
    assert!(toc_text.contains("ISRC \"USWB10100001\"\n"));
}

#[test]
fn toc_is_cdrdao_syntax() {
    let text = render_toc(&toc(), &meta(), &names(12));
    assert!(text.starts_with("CD_DA\n\nCATALOG \"0720642442524\"\n\nCD_TEXT {\n  LANGUAGE_MAP {\n    0 : EN\n  }\n  LANGUAGE 0 {\n    TITLE \"Nevermind\"\n    PERFORMER \"Nirvana\"\n  }\n}\n"), "{text}");
    assert!(text.contains("\n// Track 1\nTRACK AUDIO\nNO COPY\nNO PRE_EMPHASIS\nTWO_CHANNEL_AUDIO\nCD_TEXT {\n  LANGUAGE 0 {\n    TITLE \"T1\"\n    PERFORMER \"Nirvana\"\n  }\n}\nFILE \"01 T1.flac\" 0 05:01:18\n"), "{text}");
    assert!(text.contains("\n// Track 12\n"));
    assert!(text.contains("FILE \"12 T12.flac\" 0 03:52:65\n")); // 191880-174415 = 17465
    let mut m = meta();
    m.barcode = None;
    assert!(!render_toc(&toc(), &m, &names(12)).contains("CATALOG"));
}

#[test]
fn log_has_signature_ids_table_and_results() {
    let states = vec!["verified_ctdb"; 12];
    let log = render_log(&toc(), &meta(), &names(12), &report(12), &states);
    let lines: Vec<&str> = log.lines().collect();
    assert_eq!(lines[0], RIP_LOG_SIGNATURE);
    assert!(
        log.contains("ドライブ: ASUS BW-16D1HT 3.10 (/dev/sr0)\n"),
        "{log}"
    );
    assert!(log.contains("読み取りオフセット: +6 サンプル（学習済み）\n"));
    assert!(log.contains("試行: 1 回\n"));
    assert!(log.contains("エンコーダ: flac 1.5.0 -8 --verify\n"));
    assert!(log.contains("MusicBrainz DiscID: y6Br7t4P.bldLe_6Im2d9Z42IU4-\n"));
    assert!(log.contains("AccurateRip ID: 0013f127-00b61059-a109fe0c\n"));
    assert!(log.contains("FreeDB ID: a109fe0c\n"));
    assert!(log.contains(
        "TOC: 1 12 192030 150 22743 41850 58283 72070 91348 104618 115338 132138 143908 159828 174565\n"
    ));
    assert!(log.contains("アルバム: Nirvana / Nevermind (1991-09-24)\n"));
    assert!(log.contains("ディスク: 1 / 1\n"));
    assert!(log.contains("レーベル / カタログ番号 / バーコード: DGC / DGCD-24425 / 720642442524\n"));
    // トラック表: 番号 開始LBA 長さ 再読み ずれ C2 ARv1 ARv2 CTDB AR CTDB ファイル名
    assert!(
        log.contains(
            " 1      0 05:01:18   0   0   0 00001000 00002000 00003000 OK(12)  OK(5)   01 T1.flac\n"
        ),
        "{log}"
    );
    assert!(
        log.contains(
            " 4  58133 03:03:62   0   0   0 00001003 00002003 00003003 NG      OK(5)   04 T4.flac\n"
        ),
        "{log}"
    );
    assert!(
        log.contains("CTDB: verified（オフセット 0、信頼度 5）。修復 3 語\n"),
        "{log}"
    );
    assert!(log.contains("AccurateRip: mismatch（オフセット 0、信頼度 0）\n"));
    assert!(log.contains("結果: verified_ctdb ×12\n"));
    // ずれが無ければ凡例は出さない
    assert!(!log.contains("ずれ:"), "{log}");
    // ずれがあれば列に出し、表の下に意味を書く（照合が通らないときにジッターを疑えるように）
    let mut r = report(12);
    r.reads[1].slips = 5;
    let log = render_log(&toc(), &meta(), &names(12), &r, &["verified_ctdb"; 12]);
    assert!(log.contains(" 2  22593 "), "{log}");
    assert!(log.contains("   0   5   0 00001001"), "{log}");
    assert!(log.contains("ずれ: 5 回"), "{log}");
    assert!(!log.contains("試行ごとのずれ"), "{log}");
    r.attempt_slips = vec![3, 0, 5];
    let log = render_log(&toc(), &meta(), &names(12), &r, &["verified_ctdb"; 12]);
    assert!(log.contains("試行ごとのずれ: 3 / 0 / 5\n"), "{log}");
    // 照会しなかった手法は「照会せず」、状態が混ざれば件数を並べる。空欄は -
    let mut r = report(12);
    r.ctdb = None;
    r.accuraterip = None;
    r.repaired_words = None;
    r.drive = None;
    r.offset_source = OffsetSource::Manual;
    r.read_offset = -30;
    let mut states = vec!["not_attempted"; 12];
    states[0] = "mismatch";
    let log = render_log(&toc(), &meta(), &names(12), &r, &states);
    assert!(log.contains("ドライブ: (不明) (/dev/sr0)\n"), "{log}");
    assert!(log.contains("読み取りオフセット: -30 サンプル（手動）\n"));
    assert!(log.contains("CTDB: 照会せず\n"));
    assert!(log.contains("AccurateRip: 照会せず\n"));
    assert!(log.contains("結果: mismatch ×1, not_attempted ×11\n"));
    assert!(
        log.contains("00003000 -       -       01 T1.flac\n"),
        "{log}"
    );
    // DB に候補が無い手法は NG でなく「なし」（不一致と区別する）
    let mut r = report(12);
    if let Some(ar) = r.accuraterip.as_mut() {
        ar.outcome = spindle::cd::verify::Outcome::NotFound;
        for t in &mut ar.tracks {
            t.matched = false;
        }
    }
    let log = render_log(&toc(), &meta(), &names(12), &r, &["verified_ctdb"; 12]);
    assert!(log.contains(" なし "), "{log}");
    assert!(!log.contains(" NG "), "{log}");
}
