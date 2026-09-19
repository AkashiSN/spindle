//! CTDB の修復（SPEC §7.2「照合」、D-13 / D-66、P2-7）。CUETools の `CDRepair.cs` /
//! `RsDecode.cs` / `Parity2Syndrome.cs` に合わせた Reed-Solomon（GF(2^16)、生成多項式 0x1100B）。
//!
//! ディスクの 16 bit 語を stride 語ずつの行に並べ、列ごとに RS 符号語とみなす。データ行は先頭の
//! 1 行（leadin）と末尾の 1 行 + 端数（leadout）を除いた 1..=K 行。シンドローム S_i(c) = Σ_r d_r·α^{i(K−r)}
//! を「直接の定義」としてテスト側で独立に計算し、実装（Horner のストリーミング、ずらした列、
//! パリティ → シンドローム、復号）と突き合わせる。stride は本番 11760 語だが、テストでは小さくする

use std::ops::Range;

use spindle::cd::ctdb::CtdbEntry;
use spindle::cd::repair::{
    decode_entry_syndrome, gf16, parity_to_syndrome, DbSyndromes, RepairApplier, RepairError,
    SyndromeSampler, SyndromeTable, MAX_NPAR, STRIDE_WORDS,
};

// ---------------------------------------------------------------- テスト側の独立実装

/// GF(2^16) の乗算を筆算で（表を使わない）
fn gf_mul_naive(mut a: u32, mut b: u32) -> u16 {
    let mut p = 0u32;
    while b != 0 {
        if b & 1 != 0 {
            p ^= a;
        }
        a <<= 1;
        if a & 0x10000 != 0 {
            a ^= 0x1100B;
        }
        b >>= 1;
    }
    p as u16
}

fn gf_pow_naive(mut e: u64) -> u16 {
    e %= 65535;
    let mut r = 1u16;
    let mut base = 2u16;
    while e > 0 {
        if e & 1 != 0 {
            r = gf_mul_naive(u32::from(r), u32::from(base));
        }
        base = gf_mul_naive(u32::from(base), u32::from(base));
        e >>= 1;
    }
    r
}

struct Shape {
    stride: usize,
    rows: usize,
}

/// CUETools と同じ形: laststride = stride + (2N mod stride)、K = 2N / stride − 2
fn shape(stride: usize, total_words: usize) -> Shape {
    Shape {
        stride,
        rows: total_words / stride - 2,
    }
}

/// データ行（1..=K）の語の範囲
fn region(s: &Shape) -> Range<usize> {
    s.stride..s.stride + s.rows * s.stride
}

/// 直接の定義: S_i(c) = Σ_{r=1..K} d_r · α^{i(K−r)}、d_r = words[r·stride + c + shift]
fn naive_syndromes(words: &[u16], s: &Shape, npar: usize, shift: isize) -> Vec<Vec<u16>> {
    (0..s.stride)
        .map(|c| {
            (0..npar)
                .map(|i| {
                    let mut acc = 0u16;
                    for r in 1..=s.rows {
                        let idx = (r * s.stride + c) as isize + shift;
                        let d = words[idx as usize];
                        let e = (i * (s.rows - r)) as u64;
                        acc ^= gf_mul_naive(u32::from(d), u32::from(gf_pow_naive(e)));
                    }
                    acc
                })
                .collect()
        })
        .collect()
}

/// 系統的 RS 符号化（LFSR）: 生成多項式 G(x) = Π_{k<npar}(x + α^k)。戻りは x^(npar−1) の係数から順
fn lfsr_parity(data: &[u16], npar: usize) -> Vec<u16> {
    // G の係数（最高次から）
    let mut g = vec![0u16; npar + 1];
    g[0] = 1;
    for k in 0..npar {
        let ak = gf_pow_naive(k as u64);
        for j in (1..=k + 1).rev() {
            g[j] ^= gf_mul_naive(u32::from(g[j - 1]), u32::from(ak));
        }
        // g[0] は 1 のまま（x の最高次）
    }
    // 多項式除算: D(x)·x^npar mod G(x)
    let mut reg = vec![0u16; npar];
    for &d in data {
        let fb = reg[0] ^ d;
        for i in 0..npar {
            let next = if i + 1 < npar { reg[i + 1] } else { 0 };
            reg[i] = next ^ gf_mul_naive(u32::from(fb), u32::from(g[i + 1]));
        }
    }
    reg
}

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn word(&mut self) -> u16 {
        self.next() as u16
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn random_words(rng: &mut Rng, n: usize) -> Vec<u16> {
    (0..n).map(|_| rng.word()).collect()
}

fn to_samples(words: &[u16]) -> Vec<i16> {
    words.iter().map(|&w| w as i16).collect()
}

/// 不揃いな塊で流す
fn sample_table(words: &[u16], npar: usize, stride: usize) -> SyndromeTable {
    let samples = to_samples(words);
    let mut sampler =
        SyndromeSampler::with_stride(words.len() as u64 / 2, npar, stride).expect("sampler");
    let mut pos = 0;
    let chunks = [7usize, 1, 13, 3, 30, 5];
    let mut k = 0;
    while pos < samples.len() {
        let n = chunks[k % chunks.len()].min(samples.len() - pos);
        sampler.push(&samples[pos..pos + n]).expect("push");
        pos += n;
        k += 1;
    }
    sampler.finish().expect("finish")
}

/// CTDB の CRC（データ行の範囲。語はリトルエンディアン）
fn region_crc(words: &[u16], range: Range<usize>) -> u32 {
    let mut h = crc32fast::Hasher::new();
    for &w in &words[range] {
        h.update(&w.to_le_bytes());
    }
    h.finalize()
}

// ---------------------------------------------------------------- GF(2^16)

#[test]
fn gf16_tables_match_the_naive_arithmetic() {
    let g = gf16();
    assert_eq!(g.exp(0), 1);
    assert_eq!(g.exp(1), 2);
    assert_eq!(g.exp(16), 0x100B, "α^16 = x^12 + x^3 + x + 1");
    assert_eq!(g.exp(65535), 1, "α の位数は 65535");
    let mut rng = Rng(0x9E3779B97F4A7C15);
    for _ in 0..2000 {
        let a = rng.word();
        let b = rng.word();
        assert_eq!(g.mul(a, b), gf_mul_naive(u32::from(a), u32::from(b)));
        assert_eq!(g.mul(a, b), g.mul(b, a));
        if a != 0 {
            assert_eq!(g.mul(a, g.inv(a)), 1);
            assert_eq!(g.exp(g.log(a)), a);
        }
    }
    assert_eq!(g.mul(0, 12345), 0);
    for e in [0u64, 1, 2, 100, 65534, 65535, 70000] {
        assert_eq!(g.exp((e % 65535) as usize), gf_pow_naive(e));
    }
}

// ---------------------------------------------------------------- シンドローム表

#[test]
fn syndromes_match_the_direct_definition() {
    let mut rng = Rng(1);
    let stride = 6;
    let total_words = stride * 9 + 4; // K = 7、laststride = 6 + 4
    let words = random_words(&mut rng, total_words);
    let s = shape(stride, total_words);
    let table = sample_table(&words, 4, stride);
    assert_eq!(table.rows(), 7);
    assert_eq!(table.stride(), stride);
    assert_eq!(table.total_words(), total_words as u64);
    let expected = naive_syndromes(&words, &s, 4, 0);
    for (c, e) in expected.iter().enumerate() {
        assert_eq!(table.column(c), &e[..], "column {c}");
    }
}

#[test]
fn sampler_defaults_to_the_ctdb_stride_and_rejects_short_or_long_input() {
    assert_eq!(STRIDE_WORDS, 2 * 5880);
    assert_eq!(MAX_NPAR, 16);
    // 本番の stride: 3 行分は要る
    assert!(matches!(
        SyndromeSampler::new(5880 * 2, 8),
        Err(RepairError::TooShort { .. })
    ));
    let mut sampler = SyndromeSampler::new(5880 * 3 + 100, 8).expect("sampler");
    assert!(matches!(
        SyndromeSampler::new(5880 * 3, 0),
        Err(RepairError::BadNpar(0))
    ));
    assert!(matches!(
        SyndromeSampler::new(5880 * 3, 17),
        Err(RepairError::BadNpar(17))
    ));
    // stride は 0 と奇数を拒む。長すぎるディスク（符号長が 65535 を超える）も
    assert!(matches!(
        SyndromeSampler::with_stride(100, 8, 0),
        Err(RepairError::BadStride(0))
    ));
    assert!(matches!(
        SyndromeSampler::with_stride(100, 8, 7),
        Err(RepairError::BadStride(7))
    ));
    assert!(matches!(
        SyndromeSampler::new(5880 * 65522, 8),
        Err(RepairError::TooLong { .. })
    ));
    assert!(SyndromeSampler::new(5880 * 65521, 8).is_ok());
    assert!(matches!(
        SyndromeSampler::new(u64::MAX, 8),
        Err(RepairError::TooLong { .. })
    ));
    // 足りないまま finish、多すぎる push
    sampler.push(&[0i16; 100]).expect("push");
    assert!(matches!(
        sampler.finish(),
        Err(RepairError::TooFewSamples { .. })
    ));
    let mut sampler = SyndromeSampler::new(5880 * 3 + 100, 8).expect("sampler");
    assert!(matches!(
        sampler.push(&vec![0i16; (5880 * 3 + 101) * 2]),
        Err(RepairError::TooManySamples { .. })
    ));
}

/// ずらした列: DB のサンプル k が自分のサンプル k + offset のとき（[`spindle::cd::crctable`] と同じ向き）
#[test]
fn shifted_columns_match_recomputation_over_the_shifted_data() {
    let mut rng = Rng(2);
    let stride = 8;
    let total_words = stride * 12 + 6;
    let words = random_words(&mut rng, total_words);
    let s = shape(stride, total_words);
    let table = sample_table(&words, 6, stride);
    for offset in -3i32..=3 {
        let expected = naive_syndromes(&words, &s, 6, 2 * offset as isize);
        for (c, e) in expected.iter().enumerate() {
            assert_eq!(
                &table.column_at(c, offset).expect("column"),
                e,
                "offset {offset} column {c}"
            );
        }
    }
    // |2·offset| は stride 未満
    assert!(matches!(
        table.column_at(0, 4),
        Err(RepairError::OffsetOutOfRange(4))
    ));
    assert!(matches!(
        table.column_at(0, -4),
        Err(RepairError::OffsetOutOfRange(-4))
    ));
}

// ---------------------------------------------------------------- パリティとシンドローム

/// 旧形式の `parity` 属性（列 0 のパリティ 8 語）→ シンドローム。LFSR で作ったパリティから
/// 直接の定義のシンドロームが出る
#[test]
fn parity_to_syndrome_matches_the_direct_syndrome_of_an_encoded_column() {
    let mut rng = Rng(3);
    for &npar in &[8usize, 4, 16] {
        let data = random_words(&mut rng, 50);
        let parity = lfsr_parity(&data, npar);
        let syn = parity_to_syndrome(&parity, npar);
        let expected: Vec<u16> = (0..npar)
            .map(|i| {
                let mut acc = 0u16;
                for (r, &d) in data.iter().enumerate() {
                    let e = (i * (data.len() - 1 - r)) as u64;
                    acc ^= gf_mul_naive(u32::from(d), u32::from(gf_pow_naive(e)));
                }
                acc
            })
            .collect();
        assert_eq!(syn, expected, "npar {npar}");
    }
}

fn entry(syndrome: Option<&str>, parity: Option<&str>, npar: u32) -> CtdbEntry {
    CtdbEntry {
        id: 1,
        confidence: 1,
        crc32: 0,
        track_crcs: vec![],
        toc: "0:100".to_owned(),
        npar,
        stride: 5880,
        has_parity: None,
        syndrome: syndrome.map(str::to_owned),
        parity: parity.map(str::to_owned),
    }
}

/// 実サーバの応答（tests/fixtures/cd/ctdb_hybrid_theory.xml）: 同じディスクの npar 16 と 8 のエントリは
/// 先頭 8 語が一致する。`syndrome` はリトルエンディアン 16 bit が npar 個
#[test]
fn entry_syndrome_is_decoded_from_the_attribute() {
    let s16 = decode_entry_syndrome(&entry(
        Some("Xa0e5Jksw3/d3Iq7OCpLkAYlg/C7LYI/X10dEFlNaUo="),
        None,
        16,
    ))
    .expect("decode")
    .expect("present");
    let s8 = decode_entry_syndrome(&entry(Some("Xa0e5Jksw3/d3Iq7OCpLkA=="), None, 8))
        .expect("decode")
        .expect("present");
    assert_eq!(s16.len(), 16);
    assert_eq!(s8.len(), 8);
    assert_eq!(&s16[..8], &s8[..]);
    assert_eq!(s8[0], 0xad5d);
    assert_eq!(s8[1], 0xe41e);
    // 旧形式: parity 8 語 → 8 個のシンドローム
    let p = decode_entry_syndrome(&entry(None, Some("+gB5GOD++nnWTo2mb2l3mA=="), 8))
        .expect("decode")
        .expect("present");
    assert_eq!(p.len(), 8);
    let parity_words: Vec<u16> = [
        0xfau8, 0x00, 0x79, 0x18, 0xe0, 0xfe, 0xfa, 0x79, 0xd6, 0x4e, 0x8d, 0xa6, 0x6f, 0x69, 0x77,
        0x98,
    ]
    .chunks(2)
    .map(|b| u16::from_le_bytes([b[0], b[1]]))
    .collect();
    assert_eq!(p, parity_to_syndrome(&parity_words, 8));
    // どちらも無ければ None、壊れていればエラー
    assert_eq!(
        decode_entry_syndrome(&entry(None, None, 8)).expect("decode"),
        None
    );
    assert!(matches!(
        decode_entry_syndrome(&entry(Some("!!"), None, 8)),
        Err(RepairError::BadSyndrome(_))
    ));
    assert!(matches!(
        decode_entry_syndrome(&entry(Some("Xa0e"), None, 8)),
        Err(RepairError::BadSyndrome(_))
    ));
}

/// パリティファイル（`hasparity` の URL）は npar 面 × stride 語（リトルエンディアン）の面順。
/// 列 0 が XML の `syndrome` と一致する
#[test]
fn parity_file_is_plane_major() {
    let stride = 5;
    let npar = 3;
    let mut bytes = Vec::new();
    for i in 0..npar {
        for c in 0..stride {
            bytes.extend_from_slice(&((i * 100 + c) as u16).to_le_bytes());
        }
    }
    let db = DbSyndromes::parse(&bytes, stride, npar).expect("parse");
    assert_eq!(db.column(0), &[0, 100, 200]);
    assert_eq!(db.column(4), &[4, 104, 204]);
    assert_eq!(db.npar(), npar);
    assert_eq!(db.stride(), stride);
    // stride / npar が 0 は拒む
    assert!(matches!(
        DbSyndromes::parse(&bytes, 0, npar),
        Err(RepairError::BadStride(0))
    ));
    assert!(matches!(
        DbSyndromes::parse(&bytes, stride, 0),
        Err(RepairError::BadNpar(0))
    ));
    // 長い分は無視、短ければエラー
    bytes.push(0);
    assert!(DbSyndromes::parse(&bytes, stride, npar).is_ok());
    assert!(matches!(
        DbSyndromes::parse(&bytes[..29], stride, npar),
        Err(RepairError::BadParityFile { .. })
    ));
}

// ---------------------------------------------------------------- 修復

struct Disc {
    stride: usize,
    npar: usize,
    clean: Vec<u16>,
    shape: Shape,
}

fn disc(seed: u64, stride: usize, rows: usize, extra: usize, npar: usize) -> Disc {
    let total_words = stride * (rows + 2) + extra;
    let mut rng = Rng(seed);
    let clean = random_words(&mut rng, total_words);
    Disc {
        stride,
        npar,
        clean,
        shape: shape(stride, total_words),
    }
}

fn db_of(d: &Disc) -> DbSyndromes {
    DbSyndromes::from_table(&sample_table(&d.clean, d.npar, d.stride))
}

/// 塊ごとに適用して結果を返す
fn apply_all(applier: &mut RepairApplier, words: &[u16]) -> Vec<u16> {
    let mut samples = to_samples(words);
    let mut pos = 0;
    let chunks = [11usize, 2, 9, 1, 40];
    let mut k = 0;
    while pos < samples.len() {
        let n = chunks[k % chunks.len()].min(samples.len() - pos);
        applier.apply(&mut samples[pos..pos + n]);
        pos += n;
        k += 1;
    }
    samples.iter().map(|&s| s as u16).collect()
}

#[test]
fn repair_recovers_errors_within_the_capacity_of_each_column() {
    let d = disc(10, 6, 40, 2, 8);
    let db = db_of(&d);
    let mut rng = Rng(11);
    let mut damaged = d.clean.clone();
    // 列ごとに npar/2 = 4 個まで直せる。列 3 は 4 個、列 1 は 1 個、列 0 は 3 個（オフセット探索は列 0 の
    // 誤りが npar/2 未満のときだけ受けるので）、他は無傷
    let reg = region(&d.shape);
    let mut hits = Vec::new();
    for (c, n) in [(0usize, 3usize), (3, 4), (1, 1)] {
        let mut rows_hit = Vec::new();
        while rows_hit.len() < n {
            let r = 1 + rng.below(d.shape.rows);
            if !rows_hit.contains(&r) {
                rows_hit.push(r);
            }
        }
        for r in rows_hit {
            let idx = r * d.stride + c;
            assert!(reg.contains(&idx));
            damaged[idx] ^= 1 + rng.word() % 0xfffe;
            hits.push(idx);
        }
    }
    let table = sample_table(&damaged, d.npar, d.stride);
    // 列 0 に誤りがあってもオフセットは見つかる（誤り数付き）
    let m = table.find_offset(db.column(0), 2).expect("offset");
    assert_eq!(m.offset, 0);
    assert_eq!(m.errors, 3);
    let expected_crc = region_crc(&d.clean, reg.clone());
    let our_crc = region_crc(&damaged, reg.clone());
    assert_ne!(expected_crc, our_crc);
    let plan = table.plan(&db, 0, expected_crc, our_crc).expect("plan");
    assert_eq!(plan.offset, 0);
    assert_eq!(plan.fixes.len(), 8);
    let mut fixed_words: Vec<u64> = plan.fixes.iter().map(|f| f.word).collect();
    hits.sort_unstable();
    fixed_words.sort_unstable();
    assert_eq!(
        fixed_words,
        hits.iter().map(|&h| h as u64).collect::<Vec<_>>()
    );
    assert!(
        plan.fixes.windows(2).all(|w| w[0].word < w[1].word),
        "適用順に並ぶ"
    );
    assert_eq!(plan.crc, expected_crc);
    let mut applier = RepairApplier::new(&plan);
    let repaired = apply_all(&mut applier, &damaged);
    assert_eq!(repaired, d.clean);
    assert_eq!(applier.finish().expect("finish"), expected_crc);
}

#[test]
fn clean_data_needs_no_fixes() {
    let d = disc(12, 4, 20, 0, 4);
    let db = db_of(&d);
    let table = sample_table(&d.clean, d.npar, d.stride);
    let m = table.find_offset(db.column(0), 1).expect("offset");
    assert_eq!((m.offset, m.errors), (0, 0));
    let crc = region_crc(&d.clean, region(&d.shape));
    let plan = table.plan(&db, 0, crc, crc).expect("plan");
    assert!(plan.fixes.is_empty());
    assert_eq!(plan.crc, crc);
}

#[test]
fn too_many_errors_in_a_column_are_uncorrectable() {
    let d = disc(13, 6, 40, 2, 8);
    let db = db_of(&d);
    let mut damaged = d.clean.clone();
    for r in 1..=5 {
        damaged[r * d.stride + 2] ^= 0x1234;
    }
    let table = sample_table(&damaged, d.npar, d.stride);
    let reg = region(&d.shape);
    let err = table
        .plan(
            &db,
            0,
            region_crc(&d.clean, reg.clone()),
            region_crc(&damaged, reg),
        )
        .expect_err("uncorrectable");
    assert!(
        matches!(err, RepairError::Uncorrectable { column: 2 }),
        "{err:?}"
    );
}

#[test]
fn a_crc_that_does_not_match_after_the_fixes_is_rejected() {
    let d = disc(14, 6, 40, 2, 8);
    let db = db_of(&d);
    let mut damaged = d.clean.clone();
    damaged[7 * d.stride + 1] ^= 0x0f0f;
    let table = sample_table(&damaged, d.npar, d.stride);
    let reg = region(&d.shape);
    let our = region_crc(&damaged, reg.clone());
    let err = table
        .plan(&db, 0, region_crc(&d.clean, reg) ^ 1, our)
        .expect_err("crc");
    assert!(matches!(err, RepairError::CrcMismatch { .. }), "{err:?}");
}

/// leadin / leadout（パリティの範囲外）の誤りは直せないし、CRC の範囲外なので見えない。
/// 範囲内の誤りだけ直り、fixes はそれだけ
#[test]
fn errors_outside_the_parity_region_are_left_alone() {
    let d = disc(15, 6, 40, 2, 8);
    let db = db_of(&d);
    let mut damaged = d.clean.clone();
    damaged[2] ^= 0x5555; // leadin
    let last = damaged.len() - 1;
    damaged[last] ^= 0x5555; // leadout
    damaged[9 * d.stride + 4] ^= 0x0001; // データ行
    let table = sample_table(&damaged, d.npar, d.stride);
    let reg = region(&d.shape);
    let plan = table
        .plan(
            &db,
            0,
            region_crc(&d.clean, reg.clone()),
            region_crc(&damaged, reg),
        )
        .expect("plan");
    assert_eq!(plan.fixes.len(), 1);
    assert_eq!(plan.fixes[0].word, (9 * d.stride + 4) as u64);
    assert_eq!(plan.fixes[0].xor, 1);
}

/// オフセット付き: 自分のデータが DB より offset サンプルずれている（DB のサンプル k = 自分の k + offset）
#[test]
fn repair_works_at_a_nonzero_offset_found_from_the_first_column() {
    for &offset in &[1i32, -1, 2, -3] {
        let d = disc((20 + offset) as u64, 8, 30, 4, 8);
        let db = db_of(&d);
        let mut rng = Rng(99);
        // 自分のデータ: our[j] = clean[j − 2·offset]。はみ出す所は乱数
        let n = d.clean.len();
        let mut ours: Vec<u16> = (0..n)
            .map(|j| {
                let src = j as i64 - 2 * offset as i64;
                if src >= 0 && (src as usize) < n {
                    d.clean[src as usize]
                } else {
                    rng.word()
                }
            })
            .collect();
        // DB の行 r 列 c は自分の語 r·S + c + 2·offset
        let reg_db = region(&d.shape);
        let shift = 2 * offset as i64;
        let reg_ours = (reg_db.start as i64 + shift) as usize..(reg_db.end as i64 + shift) as usize;
        let hit0 = 5 * d.stride;
        let hit1 = 17 * d.stride + 6;
        ours[(hit0 as i64 + shift) as usize] ^= 0x8001;
        ours[(hit1 as i64 + shift) as usize] ^= 0x0042;
        let table = sample_table(&ours, d.npar, d.stride);
        let m = table.find_offset(db.column(0), 3).expect("offset");
        assert_eq!(m.offset, offset, "offset {offset}");
        assert_eq!(m.errors, 1);
        let expected_crc = region_crc(&d.clean, reg_db.clone());
        let our_crc = region_crc(&ours, reg_ours.clone());
        let plan = table
            .plan(&db, offset, expected_crc, our_crc)
            .expect("plan");
        assert_eq!(plan.offset, offset);
        let mut fixed: Vec<u64> = plan.fixes.iter().map(|f| f.word).collect();
        fixed.sort_unstable();
        assert_eq!(
            fixed,
            vec![(hit0 as i64 + shift) as u64, (hit1 as i64 + shift) as u64]
        );
        let mut applier = RepairApplier::new(&plan);
        let repaired = apply_all(&mut applier, &ours);
        assert_eq!(&repaired[reg_ours.clone()], &d.clean[reg_db.clone()]);
        assert_eq!(applier.finish().expect("finish"), expected_crc);
    }
}

#[test]
fn offset_search_prefers_an_exact_match_and_gives_up_beyond_the_column_capacity() {
    let d = disc(30, 8, 30, 4, 8);
    let db = db_of(&d);
    let table = sample_table(&d.clean, d.npar, d.stride);
    // 範囲を狭くしても 0 は見つかる
    assert_eq!(
        table
            .find_offset(db.column(0), 0)
            .map(|m| (m.offset, m.errors)),
        Some((0, 0))
    );
    // 列 0 の誤りが npar/2 個以上ならオフセットは決められない（CUETools と同じく npar/2 未満だけ受ける）
    let mut damaged = d.clean.clone();
    for r in 1..=4 {
        damaged[r * d.stride] ^= 0x0101;
    }
    let table = sample_table(&damaged, d.npar, d.stride);
    assert!(table.find_offset(db.column(0), 3).is_none());
    // 別の DB（無関係なデータ）とは一致しない
    let other = db_of(&disc(31, 8, 30, 4, 8));
    let table = sample_table(&d.clean, d.npar, d.stride);
    assert!(table.find_offset(other.column(0), 3).is_none());
}

#[test]
fn plan_rejects_mismatched_shapes() {
    let d = disc(40, 6, 40, 2, 8);
    let table = sample_table(&d.clean, d.npar, d.stride);
    let reg = region(&d.shape);
    let crc = region_crc(&d.clean, reg);
    // npar の違う DB
    let db4 = DbSyndromes::from_table(&sample_table(&d.clean, 4, d.stride));
    assert!(matches!(
        table.plan(&db4, 0, crc, crc),
        Err(RepairError::NparMismatch { .. })
    ));
    // stride の違う DB
    let other = disc(40, 4, 60, 2, 8);
    let db_other = db_of(&other);
    assert!(matches!(
        table.plan(&db_other, 0, crc, crc),
        Err(RepairError::StrideMismatch { .. })
    ));
    // 範囲外のオフセット
    let db = db_of(&d);
    assert!(matches!(
        table.plan(&db, 3, crc, crc),
        Err(RepairError::OffsetOutOfRange(3))
    ));
}

/// 適用は plan の語数ぶん流さないと終われない
#[test]
fn applier_requires_the_whole_disc() {
    let d = disc(50, 6, 40, 2, 8);
    let db = db_of(&d);
    let table = sample_table(&d.clean, d.npar, d.stride);
    let reg = region(&d.shape);
    let crc = region_crc(&d.clean, reg);
    let plan = table.plan(&db, 0, crc, crc).expect("plan");
    let mut applier = RepairApplier::new(&plan);
    let mut part = to_samples(&d.clean[..10]);
    applier.apply(&mut part);
    assert!(matches!(
        applier.finish(),
        Err(RepairError::TooFewSamples { .. })
    ));
}

/// 乱数で回す: npar 8 / 16、列ごとに 0..=npar/2 個の誤り、オフセット付き
#[test]
fn repair_random_stress() {
    for seed in 0..12u64 {
        let npar = if seed % 2 == 0 { 8 } else { 16 };
        let stride = 10;
        let d = disc(100 + seed, stride, 25, 2 * (seed % 4) as usize, npar);
        let db = db_of(&d);
        let mut rng = Rng(1000 + seed);
        let offset = (rng.below(9) as i32) - 4; // −4..=4、|2·offset| < 10
        let n = d.clean.len();
        let mut ours: Vec<u16> = (0..n)
            .map(|j| {
                let src = j as i64 - 2 * offset as i64;
                if src >= 0 && (src as usize) < n {
                    d.clean[src as usize]
                } else {
                    rng.word()
                }
            })
            .collect();
        let shift = 2 * offset as i64;
        let reg_db = region(&d.shape);
        let reg_ours = (reg_db.start as i64 + shift) as usize..(reg_db.end as i64 + shift) as usize;
        let mut expected_fixes = 0;
        for c in 0..stride {
            let errors = if c == 0 {
                rng.below(npar / 2)
            } else {
                rng.below(npar / 2 + 1)
            };
            let mut rows_hit = Vec::new();
            while rows_hit.len() < errors {
                let r = 1 + rng.below(d.shape.rows);
                if !rows_hit.contains(&r) {
                    rows_hit.push(r);
                }
            }
            for r in rows_hit {
                let idx = (r * stride + c) as i64 + shift;
                ours[idx as usize] ^= 1 + rng.word() % 0xfffe;
                expected_fixes += 1;
            }
        }
        let table = sample_table(&ours, npar, stride);
        let m = table
            .find_offset(db.column(0), 4)
            .unwrap_or_else(|| panic!("seed {seed}: offset"));
        assert_eq!(m.offset, offset, "seed {seed}");
        let expected_crc = region_crc(&d.clean, reg_db.clone());
        let our_crc = region_crc(&ours, reg_ours.clone());
        let plan = table
            .plan(&db, offset, expected_crc, our_crc)
            .unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_eq!(plan.fixes.len(), expected_fixes, "seed {seed}");
        let mut applier = RepairApplier::new(&plan);
        let repaired = apply_all(&mut applier, &ours);
        assert_eq!(
            &repaired[reg_ours.clone()],
            &d.clean[reg_db.clone()],
            "seed {seed}"
        );
        assert_eq!(applier.finish().expect("finish"), expected_crc);
    }
}

/// 本番の stride: 1 分のディスクで 1 セクタ（1176 語）が丸ごと壊れても、1176 列に 1 個ずつなので直る。
/// 直す語のセクタも報告する
#[test]
fn a_whole_bad_sector_is_repaired_at_the_real_stride() {
    let frames = 44100 * 60;
    let mut rng = Rng(7);
    let clean: Vec<u16> = (0..frames * 2).map(|_| rng.word()).collect();
    let mut db_sampler = SyndromeSampler::new(frames as u64, 8).expect("sampler");
    db_sampler.push(&to_samples(&clean)).expect("push");
    let db = DbSyndromes::from_table(&db_sampler.finish().expect("finish"));
    let mut damaged = clean.clone();
    let sector = 1234usize;
    for w in &mut damaged[sector * 1176..(sector + 1) * 1176] {
        *w ^= rng.word() | 1;
    }
    let mut sampler = SyndromeSampler::new(frames as u64, 8).expect("sampler");
    sampler.push(&to_samples(&damaged)).expect("push");
    let table = sampler.finish().expect("finish");
    assert_eq!(table.stride(), STRIDE_WORDS);
    let reg = STRIDE_WORDS..STRIDE_WORDS + table.rows() * STRIDE_WORDS;
    // 壊れたセクタは列 4704..5880 に当たり、列 0 は無傷
    let m = table.find_offset(db.column(0), 2939).expect("offset");
    assert_eq!((m.offset, m.errors), (0, 0));
    let plan = table
        .plan(
            &db,
            0,
            region_crc(&clean, reg.clone()),
            region_crc(&damaged, reg),
        )
        .expect("plan");
    assert_eq!(plan.fixes.len(), 1176);
    assert_eq!(plan.affected_sectors(), vec![sector as u64]);
    let mut applier = RepairApplier::new(&plan);
    let repaired = apply_all(&mut applier, &damaged);
    assert_eq!(repaired, clean);
}

/// 手元の目安（CI では走らせない）: `cargo test --release --test cd_repair -- --ignored bench`
#[test]
#[ignore]
fn bench_full_disc_syndromes() {
    let frames = 44100u64 * 60 * 80;
    let mut rng = Rng(3);
    let chunk: Vec<i16> = (0..588 * 2 * 64).map(|_| rng.word() as i16).collect();
    for &npar in &[8usize, 16] {
        let started = std::time::Instant::now();
        let mut sampler = SyndromeSampler::new(frames, npar).expect("sampler");
        let mut left = frames * 2;
        while left > 0 {
            let n = (chunk.len() as u64).min(left) as usize;
            sampler.push(&chunk[..n]).expect("push");
            left -= n as u64;
        }
        let table = sampler.finish().expect("finish");
        eprintln!(
            "npar {npar}: 80 分のシンドローム {:?}（rows {}）",
            started.elapsed(),
            table.rows()
        );
        let db = DbSyndromes::from_table(&table);
        let started = std::time::Instant::now();
        let m = table.find_offset(db.column(0), 2939);
        eprintln!(
            "npar {npar}: オフセット探索 ±2939 {:?} → {m:?}",
            started.elapsed()
        );
    }
}
