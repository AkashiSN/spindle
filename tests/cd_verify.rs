//! 照合の判定（SPEC §7.3、D-13、P2-9）。CRC 表と DB の応答から、オフセットを探して
//! トラックごとの一致を決める。ネットワークも DB も触らない純粋な判定
//!
//! DB の応答は合成する: 「基準の吸い出し」をディスク列 S として、自分の手元は S を o サンプル
//! ずらしたものとみなし、DB 側のエントリは S から P2-6 の直接計算で作る

use spindle::cd::accuraterip::{ArCalculator, ArDiscEntry, ArTrackEntry};
use spindle::cd::crctable::{CrcSampler, CrcTable};
use spindle::cd::ctdb::{Crc32Calculator, CtdbEntry};
use spindle::cd::toc::Toc;
use spindle::cd::verify::{match_accuraterip, match_ctdb, MethodResult, Outcome};
use spindle::cd::TrackLayout;

const SECTOR: u64 = 588;

fn lcg_pcm(seed: u32, total: u64) -> Vec<i16> {
    let mut x = seed;
    (0..total * 2)
        .map(|_| {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            (x >> 16) as u16 as i16
        })
        .collect()
}

/// S'[i] = S[i + o]
fn shifted(pcm: &[i16], o: i64) -> Vec<i16> {
    let frames = pcm.len() / 2;
    (0..frames)
        .flat_map(|i| {
            let src = i as i64 + o;
            if src < 0 || src >= frames as i64 {
                [0, 0]
            } else {
                [pcm[src as usize * 2], pcm[src as usize * 2 + 1]]
            }
        })
        .collect()
}

struct Disc {
    toc: Toc,
    layout: TrackLayout,
    /// 基準の吸い出し
    reference: Vec<i16>,
}

fn disc() -> Disc {
    let sectors = [460u64, 301, 480];
    let toc = Toc::from_audio_sample_counts(sectors.iter().map(|s| s * SECTOR)).expect("TOC");
    let layout = toc.track_layout().expect("layout");
    let reference = lcg_pcm(21, layout.total_samples());
    Disc {
        toc,
        layout,
        reference,
    }
}

fn table_of(layout: &TrackLayout, pcm: &[i16]) -> CrcTable {
    let mut s = CrcSampler::new(layout);
    s.push(pcm).expect("push");
    s.finish().expect("finish")
}

/// 基準の吸い出しから AccurateRip のエントリを作る（v1 か v2 を選べる）
fn ar_entry(d: &Disc, pcm: &[i16], confidence: u8, use_v2: bool) -> ArDiscEntry {
    let mut calc = ArCalculator::new(&d.layout);
    calc.push(pcm).expect("push");
    let crcs = calc.finish().expect("finish");
    ArDiscEntry {
        id: d.toc.accuraterip_id(),
        tracks: crcs
            .iter()
            .map(|c| ArTrackEntry {
                confidence,
                crc: if use_v2 { c.v2 } else { c.v1 },
                crc450: c.crc450.unwrap_or(0),
            })
            .collect(),
    }
}

/// 基準の吸い出しから CTDB のエントリを作る
fn ctdb_entry(d: &Disc, pcm: &[i16], confidence: u32, toc: &str) -> CtdbEntry {
    let mut calc = Crc32Calculator::new(&d.layout);
    calc.push(pcm).expect("push");
    let crcs = calc.finish().expect("finish");
    CtdbEntry {
        id: 1,
        confidence,
        crc32: crcs.disc,
        track_crcs: crcs.tracks,
        toc: toc.to_owned(),
        npar: 8,
        stride: 5880,
        has_parity: None,
        syndrome: None,
        parity: None,
    }
}

// ---------------------------------------------------------------- AccurateRip

#[test]
fn accuraterip_matches_at_offset_zero_with_v1_or_v2() {
    let d = disc();
    let table = table_of(&d.layout, &d.reference);
    let entries = vec![
        ar_entry(&d, &d.reference, 7, false),
        ar_entry(&d, &d.reference, 3, true),
    ];
    let r: MethodResult = match_accuraterip(&table, &entries);
    assert_eq!(r.outcome, Outcome::Verified);
    assert_eq!(r.offset, 0);
    // 両方のエントリが一致するので信頼度は足し合わせ
    assert_eq!(r.confidence, 10);
    assert!(r.tracks.iter().all(|t| t.matched && t.confidence == 10));
    assert_eq!(r.tracks.len(), 3);
}

/// 手元が基準より +5 サンプルずれていれば offset = 5 で一致する（v2 はオフセット 0 だけ）
#[test]
fn accuraterip_finds_the_pressing_offset() {
    let d = disc();
    let mine = shifted(&d.reference, -5); // 手元[i] = 基準[i − 5] → 基準の窓は手元の +5 から
    let table = table_of(&d.layout, &mine);
    let entries = vec![
        ar_entry(&d, &d.reference, 9, false),
        ar_entry(&d, &d.reference, 4, true),
    ];
    let r = match_accuraterip(&table, &entries);
    assert_eq!(r.outcome, Outcome::Verified);
    assert_eq!(r.offset, 5);
    assert_eq!(r.confidence, 9);
}

/// 1 トラックだけ壊れていれば、そのトラックだけ不一致で全体は mismatch
#[test]
fn accuraterip_reports_the_corrupted_track() {
    let d = disc();
    let mut mine = d.reference.clone();
    let t1_start = (460 * SECTOR * 2) as usize;
    mine[t1_start + 1000] ^= 0x0001;
    let table = table_of(&d.layout, &mine);
    let entries = vec![ar_entry(&d, &d.reference, 5, false)];
    let r = match_accuraterip(&table, &entries);
    assert_eq!(r.outcome, Outcome::Mismatch);
    assert_eq!(r.offset, 0);
    assert_eq!(
        r.tracks.iter().map(|t| t.matched).collect::<Vec<_>>(),
        vec![true, false, true]
    );
    assert_eq!(r.confidence, 0);
    assert_eq!(r.tracks[0].confidence, 5);
    assert_eq!(r.tracks[1].confidence, 0);
}

#[test]
fn accuraterip_without_entries_is_not_found() {
    let d = disc();
    let table = table_of(&d.layout, &d.reference);
    let r = match_accuraterip(&table, &[]);
    assert_eq!(r.outcome, Outcome::NotFound);
    assert_eq!(r.tracks.len(), 3);
    assert!(r.tracks.iter().all(|t| !t.matched));
}

/// トラック数が違うエントリは無視する
#[test]
fn accuraterip_ignores_entries_with_a_different_track_count() {
    let d = disc();
    let table = table_of(&d.layout, &d.reference);
    let mut e = ar_entry(&d, &d.reference, 5, false);
    e.tracks.pop();
    e.id.audio_tracks = 2;
    let r = match_accuraterip(&table, &[e]);
    assert_eq!(r.outcome, Outcome::NotFound);
}

/// 一致するオフセットが複数あれば、一致トラック数 → 信頼度の順で選ぶ。
/// 同じプレスの 2 エントリが offset 0 と 5 に分かれていても、全部一致する方を採る
#[test]
fn accuraterip_prefers_the_offset_that_matches_more_tracks() {
    let d = disc();
    let table = table_of(&d.layout, &d.reference);
    // 基準を +5 ずらした「別プレス」のエントリ（全トラック一致、conf 2）と
    // 同じプレスだが 1 トラック壊れたエントリ（conf 50）
    let other = shifted(&d.reference, 5);
    let mut damaged = ar_entry(&d, &d.reference, 50, false);
    damaged.tracks[2].crc ^= 1;
    let entries = vec![ar_entry(&d, &other, 2, false), damaged];
    let r = match_accuraterip(&table, &entries);
    assert_eq!(r.outcome, Outcome::Verified);
    // other[j] = 基準[j + 5] = 手元[j + 5] なので DB の窓は手元の +5 から
    assert_eq!(r.offset, 5);
    assert_eq!(r.confidence, 2);
}

// ---------------------------------------------------------------- CTDB

#[test]
fn ctdb_matches_by_disc_crc_and_track_crcs() {
    let d = disc();
    let table = table_of(&d.layout, &d.reference);
    let toc = d.toc.ctdb_toc();
    let entries = vec![
        ctdb_entry(&d, &d.reference, 100, &toc),
        ctdb_entry(&d, &d.reference, 20, &toc),
    ];
    let r = match_ctdb(&table, &d.toc, &entries);
    assert_eq!(r.outcome, Outcome::Verified);
    assert_eq!(r.offset, 0);
    assert_eq!(r.confidence, 120);
    assert!(r.tracks.iter().all(|t| t.matched));
}

#[test]
fn ctdb_finds_the_pressing_offset_by_track_crcs() {
    let d = disc();
    let mine = shifted(&d.reference, -12);
    let table = table_of(&d.layout, &mine);
    let toc = d.toc.ctdb_toc();
    let entries = vec![ctdb_entry(&d, &d.reference, 30, &toc)];
    let r = match_ctdb(&table, &d.toc, &entries);
    assert_eq!(r.outcome, Outcome::Verified);
    assert_eq!(r.offset, 12);
    assert_eq!(r.confidence, 30);
}

/// 音声部分の長さが違う TOC のエントリ（fuzzy で返る別リリース）は候補にしない
#[test]
fn ctdb_ignores_entries_whose_toc_has_a_different_audio_length() {
    let d = disc();
    let table = table_of(&d.layout, &d.reference);
    assert_eq!(d.toc.ctdb_toc(), "0:460:761:1241");
    let entries = vec![ctdb_entry(&d, &d.reference, 30, "0:460:761:1242")];
    let r = match_ctdb(&table, &d.toc, &entries);
    assert_eq!(r.outcome, Outcome::NotFound);
    // データトラック付きでも音声部分が同じなら候補（Enhanced CD の別 TOC 表現。
    // 音声の終端 = データトラック開始 − 11400 = 1241）
    let entries = vec![ctdb_entry(&d, &d.reference, 30, "0:460:761:-12641:20000")];
    let r = match_ctdb(&table, &d.toc, &entries);
    assert_eq!(r.outcome, Outcome::Verified);
}

#[test]
fn ctdb_reports_the_corrupted_track() {
    let d = disc();
    let mut mine = d.reference.clone();
    mine[100_000] ^= 0x0100;
    let table = table_of(&d.layout, &mine);
    let toc = d.toc.ctdb_toc();
    let entries = vec![ctdb_entry(&d, &d.reference, 30, &toc)];
    let r = match_ctdb(&table, &d.toc, &entries);
    assert_eq!(r.outcome, Outcome::Mismatch);
    assert_eq!(
        r.tracks.iter().map(|t| t.matched).collect::<Vec<_>>(),
        vec![false, true, true]
    );
}

/// 自分の CRC（オフセット 0）が結果に載る（DB へ記録し、verify.log に書く）
#[test]
fn results_carry_our_own_crcs() {
    let d = disc();
    let table = table_of(&d.layout, &d.reference);
    let ar = match_accuraterip(&table, &[]);
    assert_eq!(ar.tracks[0].crc, table.ar_v1(0, 0).expect("v1"));
    assert_eq!(ar.tracks[0].crc_v2, Some(table.ar_v2(0)));
    let ctdb = match_ctdb(&table, &d.toc, &[]);
    assert_eq!(ctdb.tracks[1].crc, table.ctdb_track(1, 0).expect("crc"));
    assert_eq!(ctdb.tracks[1].crc_v2, None);
}
