//! 吸い出しの記録（`RipReport`）と、アルバムディレクトリに置く同梱ファイルの描画
//! （SPEC §5 / §7.2、D-67、P2-8）。
//!
//! - `rip.log`: 自前の人間可読テキスト。先頭行は [`RIP_LOG_SIGNATURE`] で、スキャナはこれを見て
//!   同じディレクトリに新規登録する行を `source_type = 'cd_rip'` にする（DB を消しても出自が戻る）
//! - `disc.cue`: EAC 流の複数ファイル cue。ギャップは前トラックの末尾に付く形で INDEX 00 は書かない
//!   （TOC からは分からない）。データトラックは `REM` で記す
//! - `disc.toc`: `Toc` から cdrdao 構文で生成する。実ドライブの `cdrdao read-toc` も `Toc` にして
//!   から同じ形で書く（形式を 1 つに）
//!
//! 名前は 1 枚なら `disc.cue` / `disc.toc` / `rip.log`、複数枚組は `disc<N>.cue` / `disc<N>.toc` /
//! `rip<N>.log`（[`companion_names`]）。`RipReport` は吸い出し（P2-5）が埋め、配置（`cd::place`）が
//! ここで描画する

use std::fmt::Write as _;

use serde::{Deserialize, Serialize};

use super::metadata::DiscMetadata;
use super::toc::Toc;
use super::verify::{track_state, MethodResult, Outcome};
use crate::db::verify::{DiscRecord, DiscResult, Method, MethodRecord, TrackRecord};
use crate::jobs::handlers::backup::format_utc;
use crate::media::artwork::cover_rank;

/// rip.log の先頭行（スキャナの判定キー。変えたら `is_spindle_rip_log` も見直す）
pub const RIP_LOG_SIGNATURE: &str = "spindle rip log v1";

/// 読み取りオフセットの出所（D-83）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OffsetSource {
    /// 設定（`[rip].drive_offset` の整数）
    Manual,
    /// 前に照合が通った盤で覚えた値（`drive_offsets`）
    Learned,
    /// AccurateRip のドライブ別オフセット表（`DriveOffsets.bin`。型番から。D-83 追記）
    Table,
    /// この盤の照合で見つけた値（PCM に当ててから配置した）
    Detected,
    /// 分からない（0 のまま。照合の候補が無い盤）
    Unknown,
}

impl OffsetSource {
    fn label(self) -> &'static str {
        match self {
            OffsetSource::Manual => "手動",
            OffsetSource::Learned => "学習済み",
            OffsetSource::Table => "AccurateRip のドライブ表",
            OffsetSource::Detected => "この盤の照合で検出",
            OffsetSource::Unknown => "不明",
        }
    }
}

/// 音声トラック 1 本の読み取り統計
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackRead {
    pub rereads: u32,
    pub c2_errors: u32,
    /// cd-paranoia が読み取り位置のずれを検出・補正した回数（drift / dropped / duped）。ドライブの
    /// ジッターが大きいと増える。補正の通知なので回数だけでは誤りを意味しないが、多発と照合の不一致が
    /// 併発した（P2-5 の調査）。これより前の記録には無い（0）
    #[serde(default)]
    pub slips: u32,
}

/// 音声トラック 1 本の CRC（オフセット 0）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackCrcs {
    pub ar_v1: u32,
    pub ar_v2: u32,
    pub ctdb: u32,
}

/// 吸い出しの記録。`reads` / `crcs` は音声トラック順で TOC と同じ長さ、`ctdb` / `accuraterip` の
/// `tracks` も同じ順。Inbox のサイドカー（`import::sidecar::RipEntry`）に JSON で載る
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RipReport {
    /// ドライブの型番（INQUIRY）。分からなければ None
    pub drive: Option<String>,
    pub device: String,
    /// 適用した読み取りオフセット（サンプル）
    pub read_offset: i32,
    pub offset_source: OffsetSource,
    pub started_at: i64,
    pub finished_at: i64,
    /// 吸い出しの試行回数（1 = 再リップなし）
    pub attempts: u32,
    /// 試行ごとの読み取り位置のずれの合計（[`TrackRead::slips`] の和。`reads` は最後の回だけなので、どの回で
    /// 何回起きたかはここで見る）。これより前の記録には無い（空）
    #[serde(default)]
    pub attempt_slips: Vec<u32>,
    /// エンコーダとオプション（`flac 1.5.0 -8 --verify` 等）
    pub encoder: String,
    pub reads: Vec<TrackRead>,
    pub crcs: Vec<TrackCrcs>,
    /// None = 照会しなかった
    pub ctdb: Option<MethodResult>,
    pub accuraterip: Option<MethodResult>,
    /// CTDB の修復で直した語数（None = 修復なし）
    pub repaired_words: Option<u64>,
}

impl RipReport {
    /// 検証の記録（手法ごと。照会していない手法は行を作らない）。`track_ids` は音声トラック順
    /// （`crcs` と同じ順）の登録済みの行。`tracks.verification` の写像は verify ジョブと同じ
    /// （[`track_state`]）
    pub fn disc_record(&self, disc_no: i64, track_ids: &[i64]) -> DiscRecord {
        let disc_result = |m: &MethodResult| match m.outcome {
            Outcome::Verified => DiscResult::Verified,
            Outcome::Mismatch => DiscResult::Mismatch,
            Outcome::NotFound => DiscResult::NotFound,
        };
        let crc = |i: usize| self.crcs.get(i).copied().unwrap_or_default();
        let mut methods = Vec::new();
        if let Some(m) = &self.ctdb {
            methods.push(MethodRecord {
                method: Method::Ctdb,
                result: disc_result(m),
                detected_offset: Some(m.offset),
                confidence: Some(m.confidence),
                tracks: track_ids
                    .iter()
                    .enumerate()
                    .map(|(i, &id)| TrackRecord {
                        track_id: id,
                        crc_v1: None,
                        crc_v2: None,
                        ctdb_crc: Some(crc(i).ctdb),
                        matched: m.tracks.get(i).is_some_and(|v| v.matched),
                    })
                    .collect(),
            });
        }
        if let Some(m) = &self.accuraterip {
            methods.push(MethodRecord {
                method: Method::AccurateRip,
                result: disc_result(m),
                detected_offset: Some(m.offset),
                confidence: Some(m.confidence),
                tracks: track_ids
                    .iter()
                    .enumerate()
                    .map(|(i, &id)| TrackRecord {
                        track_id: id,
                        crc_v1: Some(crc(i).ar_v1),
                        crc_v2: Some(crc(i).ar_v2),
                        ctdb_crc: None,
                        matched: m.tracks.get(i).is_some_and(|v| v.matched),
                    })
                    .collect(),
            });
        }
        let states = track_ids
            .iter()
            .enumerate()
            .filter_map(|(i, &id)| {
                track_state(self.ctdb.as_ref(), self.accuraterip.as_ref(), i).map(|s| (id, s))
            })
            .collect();
        DiscRecord {
            disc_no,
            drive_offset: Some(self.read_offset),
            methods,
            states,
        }
    }
}

/// 同梱ファイルの名前
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompanionNames {
    pub cue: String,
    pub toc: String,
    pub log: String,
}

/// 1 枚なら `disc.cue` / `disc.toc` / `rip.log`、複数枚組は `disc<N>.cue` / `disc<N>.toc` /
/// `rip<N>.log`（N = `disc_no`、0 埋めなし）
pub fn companion_names(disc_no: u8, disc_count: u8) -> CompanionNames {
    if disc_count > 1 {
        CompanionNames {
            cue: format!("disc{disc_no}.cue"),
            toc: format!("disc{disc_no}.toc"),
            log: format!("rip{disc_no}.log"),
        }
    } else {
        CompanionNames {
            cue: "disc.cue".to_owned(),
            toc: "disc.toc".to_owned(),
            log: "rip.log".to_owned(),
        }
    }
}

/// `<stem><数字 0 桁以上>.<ext>`（大小文字を区別しない）か
fn stem_number_ext(name: &str, stem: &str, ext: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix(stem) else {
        return false;
    };
    let Some(digits) = rest.strip_suffix(ext) else {
        return false;
    };
    digits.bytes().all(|b| b.is_ascii_digit())
}

/// `rip.log` / `rip<N>.log` か
pub fn is_rip_log_name(name: &str) -> bool {
    stem_number_ext(name, "rip", ".log")
}

/// album 全体の移動に追随させる同梱ファイルか: 同梱カバー画像（`cover_rank`）、`disc*.cue` /
/// `disc*.toc`、`rip*.log`
pub fn is_companion_name(name: &str) -> bool {
    cover_rank(name).is_some()
        || stem_number_ext(name, "disc", ".cue")
        || stem_number_ext(name, "disc", ".toc")
        || is_rip_log_name(name)
}

/// セクタ数を `MM:SS:FF`（75 フレーム = 1 秒）で
pub fn msf(sectors: u32) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        sectors / 4500,
        (sectors / 75) % 60,
        sectors % 75
    )
}

/// バーコードが数字だけで 12〜13 桁なら 13 桁（EAN）に左 0 埋めして返す
fn catalog_of(meta: &DiscMetadata) -> Option<String> {
    let b = meta.barcode.as_deref()?.trim();
    if b.is_empty() || !b.bytes().all(|c| c.is_ascii_digit()) || !(12..=13).contains(&b.len()) {
        return None;
    }
    Some(format!("{b:0>13}"))
}

/// cue の引用符内（規格が無いので二重引用符を落とす）
fn cue_quote(s: &str) -> String {
    s.replace('"', "")
}

/// cdrdao の文字列（`\"` でエスケープ）
fn toc_quote(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// TOC のトラック順に、音声トラックは `(index, 番号, 開始 LBA, セクタ数)`、データトラックは
/// `(番号, 開始 LBA)` を返す
enum Entry {
    Audio {
        index: usize,
        number: u8,
        start: u32,
        sectors: u32,
    },
    Data {
        number: u8,
        start: u32,
    },
}

fn entries(toc: &Toc) -> Vec<Entry> {
    let sectors = toc.audio_track_sectors();
    let mut out = Vec::with_capacity(toc.tracks().len());
    let mut index = 0;
    for t in toc.tracks() {
        if t.is_audio {
            let n = sectors
                .iter()
                .find(|(number, _)| *number == t.number)
                .map(|(_, n)| *n)
                .unwrap_or(0);
            out.push(Entry::Audio {
                index,
                number: t.number,
                start: t.start_lba,
                sectors: n,
            });
            index += 1;
        } else {
            out.push(Entry::Data {
                number: t.number,
                start: t.start_lba,
            });
        }
    }
    out
}

fn file_name_at(file_names: &[String], index: usize) -> &str {
    file_names.get(index).map(String::as_str).unwrap_or("")
}

fn first_isrc(meta: &DiscMetadata, index: usize) -> Option<&str> {
    meta.tracks
        .get(index)
        .and_then(|t| t.mb.as_ref())
        .and_then(|mb| mb.isrcs.first())
        .map(String::as_str)
        .filter(|s| !s.trim().is_empty())
}

/// EAC 流の複数ファイル cue。`file_names` は音声トラック順のファイル名
pub fn render_cue(toc: &Toc, meta: &DiscMetadata, file_names: &[String]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "REM DISCID {}", toc.musicbrainz_disc_id());
    if let Some(d) = meta
        .date
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
    {
        let _ = writeln!(s, "REM DATE {d}");
    }
    s.push_str("REM COMMENT \"spindle\"\n");
    if let Some(c) = catalog_of(meta) {
        let _ = writeln!(s, "CATALOG {c}");
    }
    let _ = writeln!(s, "PERFORMER \"{}\"", cue_quote(meta.album_artist.trim()));
    let _ = writeln!(s, "TITLE \"{}\"", cue_quote(meta.album.trim()));
    for e in entries(toc) {
        match e {
            Entry::Audio { index, number, .. } => {
                let _ = writeln!(
                    s,
                    "FILE \"{}\" WAVE",
                    cue_quote(file_name_at(file_names, index))
                );
                let _ = writeln!(s, "  TRACK {number:02} AUDIO");
                let title = meta.tracks.get(index).map(|t| t.title.trim()).unwrap_or("");
                let _ = writeln!(s, "    TITLE \"{}\"", cue_quote(title));
                let _ = writeln!(
                    s,
                    "    PERFORMER \"{}\"",
                    cue_quote(meta.track_artist(index))
                );
                if let Some(isrc) = first_isrc(meta, index) {
                    let _ = writeln!(s, "    ISRC {}", isrc.trim());
                }
                s.push_str("    INDEX 01 00:00:00\n");
            }
            Entry::Data { number, start } => {
                let _ = writeln!(s, "REM DATA TRACK {number} LBA {start}");
            }
        }
    }
    s
}

/// cdrdao 構文の TOC。`FILE` は各トラックのファイルを長さ付きで指す
pub fn render_toc(toc: &Toc, meta: &DiscMetadata, file_names: &[String]) -> String {
    let mut s = String::from("CD_DA\n");
    if let Some(c) = catalog_of(meta) {
        let _ = write!(s, "\nCATALOG \"{c}\"\n");
    }
    let _ = write!(
        s,
        "\nCD_TEXT {{\n  LANGUAGE_MAP {{\n    0 : EN\n  }}\n  LANGUAGE 0 {{\n    TITLE \"{}\"\n    PERFORMER \"{}\"\n  }}\n}}\n",
        toc_quote(meta.album.trim()),
        toc_quote(meta.album_artist.trim())
    );
    for e in entries(toc) {
        match e {
            Entry::Audio {
                index,
                number,
                sectors,
                ..
            } => {
                let title = meta.tracks.get(index).map(|t| t.title.trim()).unwrap_or("");
                let _ = write!(
                    s,
                    "\n// Track {number}\nTRACK AUDIO\nNO COPY\nNO PRE_EMPHASIS\nTWO_CHANNEL_AUDIO\nCD_TEXT {{\n  LANGUAGE 0 {{\n    TITLE \"{}\"\n    PERFORMER \"{}\"\n  }}\n}}\n",
                    toc_quote(title),
                    toc_quote(meta.track_artist(index))
                );
                if let Some(isrc) = first_isrc(meta, index) {
                    let _ = writeln!(s, "ISRC \"{}\"", toc_quote(isrc.trim()));
                }
                let _ = writeln!(
                    s,
                    "FILE \"{}\" 0 {}",
                    toc_quote(file_name_at(file_names, index)),
                    msf(sectors)
                );
            }
            Entry::Data { number, start } => {
                let _ = write!(s, "\n// data track {number} at LBA {start} (not ripped)\n");
            }
        }
    }
    s
}

fn outcome_label(o: Outcome) -> &'static str {
    match o {
        Outcome::Verified => "verified",
        Outcome::Mismatch => "mismatch",
        Outcome::NotFound => "not_found",
    }
}

/// 表の照合列: `OK(信頼度)` / `NG`（候補はあるが不一致）/ `なし`（DB に候補が無い）/ `-`（照会せず）
fn verdict_cell(m: Option<&MethodResult>, index: usize) -> String {
    if m.is_some_and(|m| m.outcome == Outcome::NotFound) {
        return "なし".to_owned();
    }
    match m.and_then(|m| m.tracks.get(index)) {
        Some(v) if v.matched => format!("OK({})", v.confidence),
        Some(_) => "NG".to_owned(),
        None => "-".to_owned(),
    }
}

/// rip.log の本文。`states` は音声トラックごとの `tracks.verification` の値
pub fn render_log(
    toc: &Toc,
    meta: &DiscMetadata,
    file_names: &[String],
    report: &RipReport,
    states: &[&str],
) -> String {
    let mut s = String::new();
    s.push_str(RIP_LOG_SIGNATURE);
    s.push('\n');
    let _ = writeln!(
        s,
        "日時: {} 〜 {}",
        format_utc(report.started_at),
        format_utc(report.finished_at)
    );
    let _ = writeln!(
        s,
        "ドライブ: {} ({})",
        report.drive.as_deref().unwrap_or("(不明)"),
        report.device
    );
    let _ = writeln!(
        s,
        "読み取りオフセット: {:+} サンプル（{}）",
        report.read_offset,
        report.offset_source.label()
    );
    let _ = writeln!(s, "試行: {} 回", report.attempts);
    if report.attempt_slips.iter().any(|&n| n > 0) {
        let per: Vec<String> = report.attempt_slips.iter().map(u32::to_string).collect();
        let _ = writeln!(s, "試行ごとのずれ: {}", per.join(" / "));
    }
    let _ = writeln!(s, "エンコーダ: {}", report.encoder);
    s.push('\n');
    let _ = writeln!(s, "MusicBrainz DiscID: {}", toc.musicbrainz_disc_id());
    let _ = writeln!(s, "TOC: {}", toc.musicbrainz_toc());
    let _ = writeln!(s, "AccurateRip ID: {}", toc.accuraterip_id());
    let _ = writeln!(s, "CTDB TOCID: {}", toc.ctdb_toc_id());
    let _ = writeln!(s, "CTDB TOC: {}", toc.ctdb_toc());
    let _ = writeln!(s, "FreeDB ID: {:08x}", toc.freedb_id());
    s.push('\n');
    let date = meta
        .date
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(|d| format!(" ({d})"))
        .unwrap_or_default();
    let _ = writeln!(
        s,
        "アルバム: {} / {}{date}",
        meta.album_artist.trim(),
        meta.album.trim()
    );
    let _ = writeln!(s, "ディスク: {} / {}", meta.disc_no, meta.disc_count);
    let dash = |v: Option<&String>| -> String {
        v.map(|x| x.trim())
            .filter(|x| !x.is_empty())
            .unwrap_or("-")
            .to_owned()
    };
    let _ = writeln!(
        s,
        "レーベル / カタログ番号 / バーコード: {} / {} / {}",
        dash(meta.label.as_ref()),
        dash(meta.catalog_number.as_ref()),
        dash(meta.barcode.as_ref())
    );
    if let Some(r) = meta.release_id.as_deref().filter(|r| !r.trim().is_empty()) {
        let _ = writeln!(s, "MusicBrainz release: {}", r.trim());
    }
    s.push('\n');
    s.push_str(
        " No    LBA 長さ     再読 ずれ C2  ARv1     ARv2     CTDB     AR      CTDB    ファイル\n",
    );
    for e in entries(toc) {
        match e {
            Entry::Audio {
                index,
                number,
                start,
                sectors,
            } => {
                let read = report.reads.get(index).cloned().unwrap_or_default();
                let crc = report.crcs.get(index).copied().unwrap_or_default();
                let _ = writeln!(
                    s,
                    "{:>2} {:>6} {} {:>3} {:>3} {:>3} {:08x} {:08x} {:08x} {:<7} {:<7} {}",
                    number,
                    start,
                    msf(sectors),
                    read.rereads,
                    read.slips,
                    read.c2_errors,
                    crc.ar_v1,
                    crc.ar_v2,
                    crc.ctdb,
                    verdict_cell(report.accuraterip.as_ref(), index),
                    verdict_cell(report.ctdb.as_ref(), index),
                    file_name_at(file_names, index)
                );
            }
            Entry::Data { number, start } => {
                let _ = writeln!(s, "{number:>2} {start:>6} データトラック（吸い出さない）");
            }
        }
    }
    let slips: u32 = report.reads.iter().map(|r| r.slips).sum();
    if slips > 0 {
        let _ = writeln!(
            s,
            "ずれ: {slips} 回（cd-paranoia が読み取り位置のずれ = ドライブのジッターを検出・補正した回数。\
             多いうえに照合が通らなければドライブを疑う）"
        );
    }
    s.push('\n');
    match &report.ctdb {
        Some(m) => {
            let _ = write!(
                s,
                "CTDB: {}（オフセット {}、信頼度 {}）",
                outcome_label(m.outcome),
                m.offset,
                m.confidence
            );
            if let Some(n) = report.repaired_words {
                let _ = write!(s, "。修復 {n} 語");
            }
            s.push('\n');
        }
        None => s.push_str("CTDB: 照会せず\n"),
    }
    match &report.accuraterip {
        Some(m) => {
            let _ = writeln!(
                s,
                "AccurateRip: {}（オフセット {}、信頼度 {}）",
                outcome_label(m.outcome),
                m.offset,
                m.confidence
            );
        }
        None => s.push_str("AccurateRip: 照会せず\n"),
    }
    let order = [
        "verified_ctdb",
        "verified_ar",
        "mismatch",
        "not_attempted",
        "unverifiable",
    ];
    let counts: Vec<String> = order
        .iter()
        .filter_map(|k| {
            let n = states.iter().filter(|s| *s == k).count();
            (n > 0).then(|| format!("{k} ×{n}"))
        })
        .collect();
    let _ = writeln!(s, "結果: {}", counts.join(", "));
    s
}
