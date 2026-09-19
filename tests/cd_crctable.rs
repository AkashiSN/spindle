//! オフセット付き CRC 表（SPEC §7.3、P2-9）。1 回流したサンプルから、任意のオフセット
//! （±(5×588−1)）でのトラック CRC を出す。
//!
//! 正しさは「ディスク全体を o サンプルずらした列に対して P2-6 の直接計算（オフセット 0）を
//! 走らせた値」と一致することで確かめる。ずらした列 S'[i] = S[i + o] は除外規則のおかげで
//! ディスクの外を参照しない

use spindle::cd::accuraterip::ArCalculator;
use spindle::cd::crctable::{CrcSampler, CrcTable, MAX_OFFSET};
use spindle::cd::ctdb::{crc32_combine, Crc32Calculator};
use spindle::cd::TrackLayout;

const SECTOR: u64 = 588;

fn layout(lengths: &[u64]) -> TrackLayout {
    TrackLayout::from_sample_counts(lengths.iter().copied()).expect("正しいレイアウト")
}

fn lcg_pcm(seed: u32, total: u64) -> Vec<i16> {
    let mut x = seed;
    (0..total * 2)
        .map(|_| {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            (x >> 16) as u16 as i16
        })
        .collect()
}

/// S'[i] = S[i + o]。外は 0
fn shifted(pcm: &[i16], o: i64) -> Vec<i16> {
    let frames = pcm.len() / 2;
    (0..frames)
        .flat_map(|i| {
            let src = i as i64 + o;
            if src < 0 || src >= frames as i64 {
                [0, 0]
            } else {
                let s = src as usize;
                [pcm[s * 2], pcm[s * 2 + 1]]
            }
        })
        .collect()
}

fn table(lay: &TrackLayout, pcm: &[i16]) -> CrcTable {
    let mut s = CrcSampler::new(lay);
    s.push(pcm).expect("push");
    s.finish().expect("finish")
}

/// 3 トラック。crc450 が全オフセットで有効になるよう 451 セクタ + 2939 以上
fn long_disc() -> (TrackLayout, Vec<i16>) {
    let lengths = [460 * SECTOR, 456 * SECTOR + 7, 470 * SECTOR];
    let lay = layout(&lengths);
    let pcm = lcg_pcm(11, lay.total_samples());
    (lay, pcm)
}

#[test]
fn table_agrees_with_the_direct_calculators_at_offset_zero() {
    let (lay, pcm) = long_disc();
    let t = table(&lay, &pcm);
    let mut ar = ArCalculator::new(&lay);
    ar.push(&pcm).expect("push");
    let ar = ar.finish().expect("finish");
    let mut ctdb = Crc32Calculator::new(&lay);
    ctdb.push(&pcm).expect("push");
    let ctdb = ctdb.finish().expect("finish");
    for (i, a) in ar.iter().enumerate() {
        assert_eq!(t.ar_v1(i, 0), Some(a.v1), "v1 t{i}");
        assert_eq!(t.ar_v2(i), a.v2, "v2 t{i}");
        assert_eq!(t.ar_crc450(i, 0), a.crc450, "crc450 t{i}");
        assert_eq!(t.ctdb_track(i, 0), Some(ctdb.tracks[i]), "ctdb t{i}");
    }
    assert_eq!(t.ctdb_disc(0), Some(ctdb.disc));
}

#[test]
fn table_at_offset_o_equals_direct_calculation_on_the_shifted_disc() {
    let (lay, pcm) = long_disc();
    let t = table(&lay, &pcm);
    for o in [-MAX_OFFSET, -1000, -1, 1, 7, 588, MAX_OFFSET] {
        let s = shifted(&pcm, i64::from(o));
        let mut ar = ArCalculator::new(&lay);
        ar.push(&s).expect("push");
        let ar = ar.finish().expect("finish");
        let mut ctdb = Crc32Calculator::new(&lay);
        ctdb.push(&s).expect("push");
        let ctdb = ctdb.finish().expect("finish");
        for (i, a) in ar.iter().enumerate() {
            assert_eq!(t.ar_v1(i, o), Some(a.v1), "v1 t{i} o{o}");
            assert_eq!(t.ar_crc450(i, o), a.crc450, "crc450 t{i} o{o}");
            assert_eq!(t.ctdb_track(i, o), Some(ctdb.tracks[i]), "ctdb t{i} o{o}");
        }
        assert_eq!(t.ctdb_disc(o), Some(ctdb.disc), "disc o{o}");
    }
}

/// 小さなディスクで −2939..=2939 の全オフセットを直接計算と突き合わせる（セグメントの端の全位置）
#[test]
fn every_offset_in_range_matches_the_direct_calculation() {
    // 先頭は CTDB の頭 5880 より長く、末尾は尻 (5880 + N mod 5880) より長く。中間は短い
    let lengths = [7000u64, 3000, 13000];
    let lay = layout(&lengths);
    let pcm = lcg_pcm(13, lay.total_samples());
    let t = table(&lay, &pcm);
    for o in -MAX_OFFSET..=MAX_OFFSET {
        let s = shifted(&pcm, i64::from(o));
        let mut ar = ArCalculator::new(&lay);
        ar.push(&s).expect("push");
        let ar = ar.finish().expect("finish");
        let mut ctdb = Crc32Calculator::new(&lay);
        ctdb.push(&s).expect("push");
        let ctdb = ctdb.finish().expect("finish");
        for (i, a) in ar.iter().enumerate() {
            assert_eq!(t.ar_v1(i, o), Some(a.v1), "v1 t{i} o{o}");
            assert_eq!(t.ctdb_track(i, o), Some(ctdb.tracks[i]), "ctdb t{i} o{o}");
        }
        assert_eq!(t.ctdb_disc(o), Some(ctdb.disc), "disc o{o}");
    }
}

#[test]
fn offsets_outside_the_range_are_none() {
    let (lay, pcm) = long_disc();
    let t = table(&lay, &pcm);
    assert_eq!(t.ar_v1(0, MAX_OFFSET + 1), None);
    assert_eq!(t.ar_v1(2, -MAX_OFFSET - 1), None);
    assert_eq!(t.ctdb_track(1, MAX_OFFSET + 1), None);
    assert_eq!(t.ctdb_disc(-MAX_OFFSET - 1), None);
}

/// 短いトラック（451 セクタ未満）は crc450 を持たない。トラック本体の CRC は出る
#[test]
fn short_tracks_have_no_crc450_but_still_have_track_crcs() {
    let lengths = [20 * SECTOR, 3 * SECTOR, 30 * SECTOR];
    let lay = layout(&lengths);
    let pcm = lcg_pcm(12, lay.total_samples());
    let t = table(&lay, &pcm);
    assert_eq!(t.ar_crc450(1, 0), None);
    let s = shifted(&pcm, 5);
    let mut ar = ArCalculator::new(&lay);
    ar.push(&s).expect("push");
    let ar = ar.finish().expect("finish");
    assert_eq!(t.ar_v1(1, 5), Some(ar[1].v1));
}

#[test]
fn chunked_pushes_build_the_same_table() {
    let (lay, pcm) = long_disc();
    let whole = table(&lay, &pcm);
    let mut s = CrcSampler::new(&lay);
    let mut pos = 0usize;
    let mut step = 1usize;
    while pos < pcm.len() {
        let end = (pos + step).min(pcm.len());
        s.push(&pcm[pos..end]).expect("push");
        pos = end;
        step = step % 2311 + 1;
    }
    let chunked = s.finish().expect("finish");
    for o in [-MAX_OFFSET, 0, 13, MAX_OFFSET] {
        for i in 0..3 {
            assert_eq!(chunked.ar_v1(i, o), whole.ar_v1(i, o));
            assert_eq!(chunked.ctdb_track(i, o), whole.ctdb_track(i, o));
        }
        assert_eq!(chunked.ctdb_disc(o), whole.ctdb_disc(o));
    }
}

/// zlib の crc32_combine: crc(A‖B) = combine(crc(A), crc(B), |B|)
#[test]
fn crc32_combine_joins_two_crcs() {
    let a: Vec<u8> = (0..1000u32).map(|i| (i * 31 % 251) as u8).collect();
    let b: Vec<u8> = (0..777u32).map(|i| (i * 17 % 253) as u8).collect();
    let mut ab = a.clone();
    ab.extend_from_slice(&b);
    assert_eq!(
        crc32_combine(crc32fast::hash(&a), crc32fast::hash(&b), b.len() as u64),
        crc32fast::hash(&ab)
    );
    // 長さ 0 の結合は crc(A)
    assert_eq!(
        crc32_combine(crc32fast::hash(&a), 0, 0),
        crc32fast::hash(&a)
    );
    // 部分列: crc(B) = combine(crc(A), crc(A‖B), |B|)
    assert_eq!(
        crc32_combine(crc32fast::hash(&a), crc32fast::hash(&ab), b.len() as u64),
        crc32fast::hash(&b)
    );
}
