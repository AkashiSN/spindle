//! AccurateRip v1/v2 CRC と CTDB CRC32（SPEC §7.2「CRC 計算」、D-13）。
//! 受け入れ: docs/TASKS.md P2-6
//!
//! 除外規則（先頭トラックの頭 5×588−1、末尾トラックの尻 5×588、CTDB の 10 セクタ）は
//! 壊れても画面上は「不一致」に見えるだけで発見できないので、
//! 閉じた式で手計算できる小さな入力と、Python の独立実装
//! （scripts/gen_cd_crc_fixture.py）が出した参照値の両方で押さえる

use spindle::cd::accuraterip::{ArCalculator, TrackCrc};
use spindle::cd::ctdb::{Crc32Calculator, DiscCrc};
use spindle::cd::{CrcError, TrackLayout};

const SECTOR: u64 = 588;

// ---------------------------------------------------------------- ヘルパ

fn layout(lengths: &[u64]) -> TrackLayout {
    TrackLayout::from_sample_counts(lengths.iter().copied()).expect("正しいレイアウト")
}

/// 全サンプルが同じ (L, R) のトラック列
fn constant_pcm(lengths: &[u64], l: i16, r: i16) -> Vec<i16> {
    let total: u64 = lengths.iter().sum();
    (0..total).flat_map(|_| [l, r]).collect()
}

fn ar_crcs(lay: &TrackLayout, pcm: &[i16]) -> Vec<TrackCrc> {
    let mut calc = ArCalculator::new(lay);
    calc.push(pcm).expect("push");
    calc.finish().expect("finish")
}

fn ctdb_crcs(lay: &TrackLayout, pcm: &[i16]) -> DiscCrc {
    let mut calc = Crc32Calculator::new(lay);
    calc.push(pcm).expect("push");
    calc.finish().expect("finish")
}

/// Python 側と同じ LCG（scripts/gen_cd_crc_fixture.py）
fn lcg_pcm(seed: u32, total: u64) -> Vec<i16> {
    let mut x = seed;
    (0..total * 2)
        .map(|_| {
            x = x.wrapping_mul(1103515245).wrapping_add(12345);
            (x >> 16) as u16 as i16
        })
        .collect()
}

fn pcm_bytes(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().flat_map(|s| s.to_le_bytes()).collect()
}

// ---------------------------------------------------------------- レイアウト

#[test]
fn layout_rejects_empty_and_zero_length_tracks() {
    assert!(TrackLayout::from_sample_counts(std::iter::empty()).is_err());
    assert!(TrackLayout::from_sample_counts([588, 0, 588]).is_err());
    let lay = layout(&[588, 1176]);
    assert_eq!(lay.track_count(), 2);
    assert_eq!(lay.total_samples(), 1764);
}

// ---------------------------------------------------------------- AccurateRip: 手計算

/// 1 トラック（先頭かつ末尾）、語 = 1。頭 2939 と尻 2940 を除くと位置 2940..=2945 の 6 個が残る
#[test]
fn ar_single_track_skips_head_2939_and_tail_2940() {
    let n = 2939 + 6 + 2940;
    let lay = layout(&[n]);
    let crcs = ar_crcs(&lay, &constant_pcm(&[n], 1, 0));
    let expected: u32 = (2940..=2945).sum();
    assert_eq!(crcs.len(), 1);
    assert_eq!(crcs[0].v1, expected);
    // 積が 2^32 を超えないので v2 == v1
    assert_eq!(crcs[0].v2, expected);
}

/// 2 トラック各 3000 サンプル、語 = 1。頭の除外は 1 本目だけ、尻の除外は 2 本目だけ
#[test]
fn ar_head_skip_applies_to_first_and_tail_skip_to_last_only() {
    let lay = layout(&[3000, 3000]);
    let crcs = ar_crcs(&lay, &constant_pcm(&[3000, 3000], 1, 0));
    assert_eq!(crcs[0].v1, (2940..=3000).sum::<u32>());
    assert_eq!(crcs[1].v1, (1..=60).sum::<u32>());
}

/// 中間トラックは除外なし。位置はトラックごとに 1 から数え直す
#[test]
fn ar_middle_track_counts_all_samples_from_position_one() {
    let lay = layout(&[3000, 10, 3000]);
    let crcs = ar_crcs(&lay, &constant_pcm(&[3000, 10, 3000], 1, 0));
    assert_eq!(crcs[1].v1, (1..=10).sum::<u32>());
}

/// 語 = 0x8000_0000（L = 0, R = i16::MIN）。位置 1..=4 の積は 2^31, 2^32, 3×2^31, 2^33。
/// v1 は下位 32 bit の和 = 2^32 → 0、v2 はそれに上位 32 bit の和 (0+1+1+2) を足す
#[test]
fn ar_v2_adds_high_words_of_the_64bit_products() {
    let lay = layout(&[3000, 4, 3000]);
    let crcs = ar_crcs(&lay, &constant_pcm(&[3000, 4, 3000], 0, i16::MIN));
    assert_eq!(crcs[1].v1, 0);
    assert_eq!(crcs[1].v2, 4);
}

/// L は下位 16 bit、R は上位 16 bit。符号拡張してはいけない
#[test]
fn ar_word_packs_left_low_and_right_high_without_sign_extension() {
    let lay = layout(&[3000, 1, 3000]);
    // L = -1 (0xFFFF), R = 2 → 語 = 0x0002_FFFF、位置 1 なので v1 = 語そのもの
    let crcs = ar_crcs(&lay, &constant_pcm(&[3000, 1, 3000], -1, 2));
    assert_eq!(crcs[1].v1, 0x0002_FFFF);
}

/// crc450 は 450 セクタ目の 588 サンプルを位置 1 から数えた v1。語 = 1 なら Σ1..588
#[test]
fn ar_crc450_is_v1_of_sector_450_counted_from_one() {
    let n = 460 * SECTOR;
    let lay = layout(&[n, n]);
    let crcs = ar_crcs(&lay, &constant_pcm(&[n, n], 1, 0));
    let expected: u32 = (1..=588).sum();
    assert_eq!(crcs[0].crc450, Some(expected));
    assert_eq!(crcs[1].crc450, Some(expected));
}

/// crc450 は除外規則の影響を受けない（1 トラック 451 セクタなら窓は末尾除外の中にある）
#[test]
fn ar_crc450_ignores_the_tail_exclusion() {
    let n = 451 * SECTOR;
    let lay = layout(&[n]);
    let crcs = ar_crcs(&lay, &constant_pcm(&[n], 1, 0));
    assert_eq!(crcs[0].crc450, Some((1..=588).sum::<u32>()));
}

/// 451 セクタ未満のトラックは crc450 を持たない
#[test]
fn ar_crc450_is_none_for_tracks_shorter_than_451_sectors() {
    let n = 451 * SECTOR - 1;
    let lay = layout(&[n, 460 * SECTOR]);
    let crcs = ar_crcs(&lay, &constant_pcm(&[n, 460 * SECTOR], 1, 0));
    assert_eq!(crcs[0].crc450, None);
    assert!(crcs[1].crc450.is_some());
}

// ---------------------------------------------------------------- CTDB: 構造

/// ディスク CRC は先頭 10 セクタと末尾 (10 セクタ + 総数 mod 5880) を除いた範囲の zlib CRC32
#[test]
fn ctdb_disc_crc_strips_head_and_tail_strides() {
    // 30 セクタ + 7 サンプル。総数 mod 5880 = 7
    let lengths = [20 * SECTOR, 10 * SECTOR + 7];
    let total: u64 = lengths.iter().sum();
    let lay = layout(&lengths);
    let pcm = lcg_pcm(1, total);
    let crc = ctdb_crcs(&lay, &pcm);
    let head = 10 * SECTOR;
    let tail = 10 * SECTOR + total % (10 * SECTOR);
    assert_eq!(tail, 5880 + 7);
    let region = &pcm[(head * 2) as usize..((total - tail) * 2) as usize];
    assert_eq!(crc.disc, crc32fast::hash(&pcm_bytes(region)));
}

/// トラック CRC: 先頭は頭 10 セクタを除き、末尾は尻を除き、中間は全体
#[test]
fn ctdb_track_crcs_apply_the_disc_exclusions_at_the_ends_only() {
    // 40 セクタ + 7 サンプル。総数 mod 5880 = 7
    let lengths = [20 * SECTOR, 3 * SECTOR, 17 * SECTOR + 7];
    let total: u64 = lengths.iter().sum();
    let lay = layout(&lengths);
    let pcm = lcg_pcm(2, total);
    let crc = ctdb_crcs(&lay, &pcm);
    let head = (10 * SECTOR * 2) as usize;
    let tail = ((10 * SECTOR + total % (10 * SECTOR)) * 2) as usize;
    let t0 = &pcm[..(lengths[0] * 2) as usize];
    let t1 = &pcm[(lengths[0] * 2) as usize..((lengths[0] + lengths[1]) * 2) as usize];
    let t2 = &pcm[((lengths[0] + lengths[1]) * 2) as usize..];
    assert_eq!(crc.tracks.len(), 3);
    assert_eq!(crc.tracks[0], crc32fast::hash(&pcm_bytes(&t0[head..])));
    assert_eq!(crc.tracks[1], crc32fast::hash(&pcm_bytes(t1)));
    assert_eq!(
        crc.tracks[2],
        crc32fast::hash(&pcm_bytes(&t2[..t2.len() - tail]))
    );
}

/// 1 トラックならディスク CRC とトラック CRC は一致する
#[test]
fn ctdb_single_track_disc_and_track_crc_coincide() {
    let n = 30 * SECTOR + 100;
    let lay = layout(&[n]);
    let pcm = lcg_pcm(3, n);
    let crc = ctdb_crcs(&lay, &pcm);
    assert_eq!(crc.tracks, vec![crc.disc]);
}

// ---------------------------------------------------------------- ストリーミングと境界

/// 細切れ（奇数個の i16 を含む）で流しても 1 回で流したのと同じ
#[test]
fn chunked_pushes_give_the_same_result_as_one_push() {
    let lengths = [20 * SECTOR, 3 * SECTOR, 15 * SECTOR + 7];
    let total: u64 = lengths.iter().sum();
    let lay = layout(&lengths);
    let pcm = lcg_pcm(4, total);
    let whole_ar = ar_crcs(&lay, &pcm);
    let whole_ctdb = ctdb_crcs(&lay, &pcm);

    let mut ar = ArCalculator::new(&lay);
    let mut ctdb = Crc32Calculator::new(&lay);
    let mut pos = 0usize;
    let mut step = 1usize;
    while pos < pcm.len() {
        let end = (pos + step).min(pcm.len());
        ar.push(&pcm[pos..end]).expect("push");
        ctdb.push(&pcm[pos..end]).expect("push");
        pos = end;
        step = step % 1237 + 1; // 1..=1237 を巡回。奇数長も混ざる
    }
    assert_eq!(ar.finish().expect("finish"), whole_ar);
    assert_eq!(ctdb.finish().expect("finish"), whole_ctdb);
}

#[test]
fn finishing_short_of_the_layout_is_an_error() {
    let lay = layout(&[588, 588]);
    let pcm = lcg_pcm(5, 1000);
    let mut ar = ArCalculator::new(&lay);
    ar.push(&pcm).expect("push");
    assert!(matches!(
        ar.finish(),
        Err(CrcError::TooFewSamples {
            expected: 1176,
            received: 1000
        })
    ));
    let mut ctdb = Crc32Calculator::new(&lay);
    ctdb.push(&pcm).expect("push");
    assert!(matches!(ctdb.finish(), Err(CrcError::TooFewSamples { .. })));
}

#[test]
fn pushing_past_the_layout_is_an_error() {
    let lay = layout(&[588]);
    let pcm = lcg_pcm(6, 589);
    let mut ar = ArCalculator::new(&lay);
    assert!(matches!(
        ar.push(&pcm),
        Err(CrcError::TooManySamples { expected: 588, .. })
    ));
    let mut ctdb = Crc32Calculator::new(&lay);
    assert!(matches!(
        ctdb.push(&pcm),
        Err(CrcError::TooManySamples { .. })
    ));
}

/// 前回の push で余った L は、次の push の先頭の R と組んで 1 フレームになる
#[test]
fn a_dangling_left_sample_is_completed_by_the_next_push() {
    let lay = layout(&[588, 588]);
    let pcm = lcg_pcm(7, 1176);
    let whole = ar_crcs(&lay, &pcm);
    let mut ar = ArCalculator::new(&lay);
    ar.push(&pcm[..1]).expect("L だけ");
    ar.push(&pcm[1..]).expect("R から");
    assert_eq!(ar.finish().expect("finish"), whole);
}

/// レイアウト分を全部受け取った後に L が 1 個余ると、finish は失敗する
#[test]
fn finishing_with_a_dangling_left_sample_is_an_error() {
    let lay = layout(&[588]);
    let mut pcm = lcg_pcm(8, 588);
    pcm.push(7);
    let mut ar = ArCalculator::new(&lay);
    ar.push(&pcm).expect("余った L は持ち越し");
    assert!(matches!(ar.finish(), Err(CrcError::IncompleteFrame)));
    let mut ctdb = Crc32Calculator::new(&lay);
    ctdb.push(&pcm).expect("余った L は持ち越し");
    assert!(matches!(ctdb.finish(), Err(CrcError::IncompleteFrame)));
}

/// 1 フレーム足りないうえに L だけ余っている場合も失敗する
#[test]
fn finishing_short_with_a_dangling_left_sample_is_an_error() {
    let lay = layout(&[588]);
    let pcm = lcg_pcm(9, 588);
    let mut ar = ArCalculator::new(&lay);
    ar.push(&pcm[..1175]).expect("push");
    assert!(matches!(ar.finish(), Err(CrcError::IncompleteFrame)));
}

/// 超過を拒んだ push は状態を変えない。正しい残りを流し直せば完走する
#[test]
fn a_rejected_push_leaves_the_calculator_unchanged() {
    let lay = layout(&[588, 588]);
    let pcm = lcg_pcm(10, 1176);
    let whole_ar = ar_crcs(&lay, &pcm);
    let whole_ctdb = ctdb_crcs(&lay, &pcm);
    // 先頭 100 サンプル + 余りの L を入れてから、残り全部 + 1 サンプルを入れて拒否させる
    let mut ar = ArCalculator::new(&lay);
    let mut ctdb = Crc32Calculator::new(&lay);
    ar.push(&pcm[..201]).expect("push");
    ctdb.push(&pcm[..201]).expect("push");
    let mut too_many = pcm[201..].to_vec();
    too_many.extend_from_slice(&[1, 2]);
    assert!(matches!(
        ar.push(&too_many),
        Err(CrcError::TooManySamples { .. })
    ));
    assert!(matches!(
        ctdb.push(&too_many),
        Err(CrcError::TooManySamples { .. })
    ));
    ar.push(&pcm[201..]).expect("残り");
    ctdb.push(&pcm[201..]).expect("残り");
    assert_eq!(ar.finish().expect("finish"), whole_ar);
    assert_eq!(ctdb.finish().expect("finish"), whole_ctdb);
}

// ---------------------------------------------------------------- Python 参照実装との突き合わせ

#[test]
fn matches_the_python_reference_fixture() {
    let text = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/cd_crc_reference.json"
    ))
    .expect("フィクスチャ");
    let doc: serde_json::Value = serde_json::from_str(&text).expect("JSON");
    let cases = doc["cases"].as_array().expect("cases");
    assert!(!cases.is_empty());
    for case in cases {
        let name = case["name"].as_str().expect("name");
        let seed = case["seed"].as_u64().expect("seed") as u32;
        let lengths: Vec<u64> = case["track_samples"]
            .as_array()
            .expect("track_samples")
            .iter()
            .map(|v| v.as_u64().expect("len"))
            .collect();
        let lay = layout(&lengths);
        let pcm = lcg_pcm(seed, lay.total_samples());

        let ar = ar_crcs(&lay, &pcm);
        let expected_ar = case["accuraterip"].as_array().expect("accuraterip");
        assert_eq!(ar.len(), expected_ar.len(), "{name}: トラック数");
        for (i, (got, want)) in ar.iter().zip(expected_ar).enumerate() {
            assert_eq!(
                got.v1,
                want["v1"].as_u64().expect("v1") as u32,
                "{name} t{i} v1"
            );
            assert_eq!(
                got.v2,
                want["v2"].as_u64().expect("v2") as u32,
                "{name} t{i} v2"
            );
            let want450 = want["crc450"].as_u64().map(|v| v as u32);
            assert_eq!(got.crc450, want450, "{name} t{i} crc450");
        }

        let ctdb = ctdb_crcs(&lay, &pcm);
        let want_disc = case["ctdb"]["disc"].as_u64().expect("disc") as u32;
        assert_eq!(ctdb.disc, want_disc, "{name} ctdb disc");
        let want_tracks: Vec<u32> = case["ctdb"]["tracks"]
            .as_array()
            .expect("tracks")
            .iter()
            .map(|v| v.as_u64().expect("crc") as u32)
            .collect();
        assert_eq!(ctdb.tracks, want_tracks, "{name} ctdb tracks");
    }
}
