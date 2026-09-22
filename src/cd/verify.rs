//! 照合の判定（SPEC §7.3、D-13、P2-9）。CRC 表（[`CrcTable`]）と DB の応答から、
//! オフセットを探してトラックごとの一致を決める。ネットワークも DB も触らない。
//!
//! オフセットは ±[`MAX_OFFSET`] の全部を試し、「一致したトラック数 → 一致した信頼度の和 →
//! 0 に近い」の順で 1 つ選ぶ（吸い出しのオフセットはディスクで 1 つなので、トラックごとに
//! 別のオフセットは採らない）。CUETools はパリティでオフセットを求めるが、ここでは
//! trackcrcs の総当たり（CUETools のフォールバック経路と同じ）。
//!
//! - AccurateRip: エントリの CRC は v1 か v2 か区別されないので、オフセット 0 では両方、
//!   それ以外では v1 と比べる（v2 はオフセットに対して線形でない）
//! - CTDB: `fuzzy=1` で別リリースも返るので、音声部分の長さ（と音声トラック数）が同じ
//!   エントリだけを候補にする。ディスク CRC が一致すれば全トラック一致、そうでなければ
//!   trackcrcs で判定
//!
//! 「不一致は不良を意味しない」（D-13）。結果の解釈と表示は呼び出し側

use serde::{Deserialize, Serialize};

use super::accuraterip::ArDiscEntry;
use super::crctable::{CrcTable, MAX_OFFSET};
use super::ctdb::CtdbEntry;
use super::toc::{Toc, SESSION_GAP_SECTORS};

/// 1 手法（AccurateRip / CTDB）の結論
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// 全トラックが一致
    Verified,
    /// 候補はあるが一致しないトラックがある
    Mismatch,
    /// DB に候補が無い
    NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackVerdict {
    pub matched: bool,
    /// 選んだオフセットで一致したエントリの信頼度の和
    pub confidence: u32,
    /// 自分の CRC（オフセット 0。AccurateRip なら v1、CTDB なら CRC32）
    pub crc: u32,
    /// AccurateRip の v2（CTDB は `None`）
    pub crc_v2: Option<u32>,
}

/// トラック `index` の `tracks.verification`: CTDB 一致 → `verified_ctdb`、AccurateRip 一致 →
/// `verified_ar`、どちらかに候補があって不一致 → `mismatch`、候補なし → `None`（据え置き）。
/// verify ジョブ（§7.3）と rip の配置（§7.2）で同じ写像を使う
pub fn track_state(
    ctdb: Option<&MethodResult>,
    ar: Option<&MethodResult>,
    index: usize,
) -> Option<crate::db::verify::TrackState> {
    use crate::db::verify::TrackState;
    if ctdb
        .and_then(|m| m.tracks.get(index))
        .is_some_and(|v| v.matched)
    {
        return Some(TrackState::VerifiedCtdb);
    }
    if ar
        .and_then(|m| m.tracks.get(index))
        .is_some_and(|v| v.matched)
    {
        return Some(TrackState::VerifiedAr);
    }
    let any = [ctdb, ar]
        .into_iter()
        .flatten()
        .any(|m| m.outcome != Outcome::NotFound);
    any.then_some(TrackState::Mismatch)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MethodResult {
    pub outcome: Outcome,
    /// 選んだオフセット（DB の窓が自分のデータのどこから始まるか。サンプル）
    pub offset: i32,
    /// 全トラックの信頼度の最小（不一致があれば 0）
    pub confidence: u32,
    pub tracks: Vec<TrackVerdict>,
}

/// オフセットごとの集計から 1 つ選んで結果にまとめる。
/// `confidence_at(offset) -> トラックごとの信頼度` を全オフセットで評価する
fn choose<F>(track_count: usize, has_candidates: bool, mut confidence_at: F) -> (i32, Vec<u32>)
where
    F: FnMut(i32) -> Vec<u32>,
{
    let mut best: Option<(i32, Vec<u32>, usize, u64)> = None;
    if has_candidates {
        for offset in -MAX_OFFSET..=MAX_OFFSET {
            let conf = confidence_at(offset);
            let matched = conf.iter().filter(|&&c| c > 0).count();
            if matched == 0 {
                continue;
            }
            let total: u64 = conf.iter().map(|&c| u64::from(c)).sum();
            let better = match &best {
                None => true,
                Some((bo, _, bm, bt)) => {
                    (matched, total, std::cmp::Reverse(offset.abs()))
                        > (*bm, *bt, std::cmp::Reverse(bo.abs()))
                }
            };
            if better {
                best = Some((offset, conf, matched, total));
            }
        }
    }
    match best {
        Some((offset, conf, _, _)) => (offset, conf),
        None => (0, vec![0; track_count]),
    }
}

fn finish(
    table: &CrcTable,
    has_candidates: bool,
    offset: i32,
    conf: Vec<u32>,
    own: impl Fn(usize) -> (u32, Option<u32>),
) -> MethodResult {
    let tracks: Vec<TrackVerdict> = (0..table.track_count())
        .map(|i| {
            let (crc, crc_v2) = own(i);
            TrackVerdict {
                matched: conf[i] > 0,
                confidence: conf[i],
                crc,
                crc_v2,
            }
        })
        .collect();
    let all = tracks.iter().all(|t| t.matched);
    let outcome = if !has_candidates {
        Outcome::NotFound
    } else if all {
        Outcome::Verified
    } else {
        Outcome::Mismatch
    };
    let confidence = if all {
        tracks.iter().map(|t| t.confidence).min().unwrap_or(0)
    } else {
        0
    };
    MethodResult {
        outcome,
        offset,
        confidence,
        tracks,
    }
}

/// AccurateRip の判定
pub fn match_accuraterip(table: &CrcTable, entries: &[ArDiscEntry]) -> MethodResult {
    let n = table.track_count();
    let candidates: Vec<&ArDiscEntry> = entries.iter().filter(|e| e.tracks.len() == n).collect();
    // 自分の v1 を全オフセットで前計算（トラック × 5879）
    let v1: Vec<Vec<Option<u32>>> = (0..n)
        .map(|i| {
            (-MAX_OFFSET..=MAX_OFFSET)
                .map(|o| table.ar_v1(i, o))
                .collect()
        })
        .collect();
    let v2: Vec<u32> = (0..n).map(|i| table.ar_v2(i)).collect();
    let (offset, conf) = choose(n, !candidates.is_empty(), |o| {
        let idx = (o + MAX_OFFSET) as usize;
        (0..n)
            .map(|i| {
                let Some(mine) = v1[i][idx] else {
                    return 0;
                };
                candidates
                    .iter()
                    .filter(|e| {
                        let crc = e.tracks[i].crc;
                        crc == mine || (o == 0 && crc == v2[i])
                    })
                    .map(|e| u32::from(e.tracks[i].confidence))
                    .sum()
            })
            .collect()
    });
    finish(table, !candidates.is_empty(), offset, conf, |i| {
        (v1[i][MAX_OFFSET as usize].unwrap_or(0), Some(v2[i]))
    })
}

/// CTDB の TOC 文字列から（音声トラック数, 音声部分の長さ（セクタ））を読む。
/// 形は `start:start:…:-datastart:leadout`（[`Toc::ctdb_toc`]）。音声の直後がデータトラックなら
/// 音声部分の終端はその開始 − 11400
fn ctdb_toc_audio_shape(toc: &str) -> Option<(usize, u32)> {
    let parts: Vec<&str> = toc.split(':').collect();
    let (&leadout, tracks) = parts.split_last()?;
    let leadout: u32 = leadout.parse().ok()?;
    let tracks: Vec<(bool, u32)> = tracks
        .iter()
        .map(|t| match t.strip_prefix('-') {
            Some(data) => data.parse().ok().map(|s| (false, s)),
            None => t.parse().ok().map(|s| (true, s)),
        })
        .collect::<Option<_>>()?;
    let first = tracks.iter().position(|(audio, _)| *audio)?;
    let last = tracks.iter().rposition(|(audio, _)| *audio)?;
    let count = tracks[first..=last].iter().filter(|(a, _)| *a).count();
    let end = match tracks.get(last + 1) {
        Some((false, data_start)) => data_start.checked_sub(SESSION_GAP_SECTORS)?,
        _ => leadout,
    };
    Some((count, end.checked_sub(tracks[first].1)?))
}

/// CTDB の判定。`toc` は自分の TOC（候補の絞り込みに使う）
pub fn match_ctdb(table: &CrcTable, toc: &Toc, entries: &[CtdbEntry]) -> MethodResult {
    let n = table.track_count();
    let own_shape = ctdb_toc_audio_shape(&toc.ctdb_toc());
    let candidates: Vec<&CtdbEntry> = entries
        .iter()
        .filter(|e| e.track_crcs.len() == n && ctdb_toc_audio_shape(&e.toc) == own_shape)
        .collect();
    let track: Vec<Vec<Option<u32>>> = (0..n)
        .map(|i| {
            (-MAX_OFFSET..=MAX_OFFSET)
                .map(|o| table.ctdb_track(i, o))
                .collect()
        })
        .collect();
    let disc: Vec<Option<u32>> = (-MAX_OFFSET..=MAX_OFFSET)
        .map(|o| table.ctdb_disc(o))
        .collect();
    let (offset, conf) = choose(n, !candidates.is_empty(), |o| {
        let idx = (o + MAX_OFFSET) as usize;
        (0..n)
            .map(|i| {
                candidates
                    .iter()
                    .filter(|e| {
                        disc[idx] == Some(e.crc32) || track[i][idx] == Some(e.track_crcs[i])
                    })
                    .map(|e| e.confidence)
                    .sum()
            })
            .collect()
    });
    finish(table, !candidates.is_empty(), offset, conf, |i| {
        (track[i][MAX_OFFSET as usize].unwrap_or(0), None)
    })
}
