//! ReplayGain の解析（SPEC §6「ReplayGain の内部表現」、docs/TASKS.md P1-1）。
//! 内部表現は RG 2.0 / -18 LUFS 基準の dB。ebur128 の積分ラウドネスと true peak から
//! track / album の値を出す

use spindle::domain::replaygain::{album_loudness, gain_db, LoudnessMeter, TrackLoudness};

/// 1 kHz の正弦波（ステレオ、両チャンネル同じ振幅）。BS.1770 の K 特性は 1 kHz 付近で 0 dB
/// なので、両チャンネルに振幅 `amp` を入れると積分ラウドネスは `20*log10(amp)` LUFS になる
fn stereo_sine(amp: f32, rate: u32, secs: f32) -> Vec<f32> {
    let n = (rate as f32 * secs) as usize;
    let mut out = Vec::with_capacity(n * 2);
    for i in 0..n {
        let t = i as f32 / rate as f32;
        let v = (t * 1000.0 * std::f32::consts::TAU).sin() * amp;
        out.push(v);
        out.push(v);
    }
    out
}

fn dbfs(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

#[test]
fn gain_is_reference_minus_loudness() {
    assert_eq!(gain_db(-18.0, -18.0), 0.0);
    assert_eq!(gain_db(-23.0, -18.0), 5.0);
    assert_eq!(gain_db(-13.0, -18.0), -5.0);
    // 基準を変えても同じ式
    assert_eq!(gain_db(-23.0, -23.0), 0.0);
}

#[test]
fn track_loudness_of_stereo_sine() {
    let mut m = LoudnessMeter::new(2, 48_000).unwrap();
    m.push(&stereo_sine(dbfs(-20.0), 48_000, 5.0)).unwrap();
    let t = m.loudness();
    assert!((t.lufs - -20.0).abs() < 0.2, "lufs = {}", t.lufs);
    assert!((t.peak - 0.1).abs() < 0.005, "peak = {}", t.peak);
    assert!((t.gain(-18.0) - 2.0).abs() < 0.2);
}

#[test]
fn push_accepts_chunks_not_aligned_to_frames() {
    // 奇数長のチャンクを流しても、まとめて流したときと同じ結果になる（フレーム境界で
    // バッファリングしている）
    let pcm = stereo_sine(dbfs(-20.0), 48_000, 3.0);
    let mut whole = LoudnessMeter::new(2, 48_000).unwrap();
    whole.push(&pcm).unwrap();
    let mut chunked = LoudnessMeter::new(2, 48_000).unwrap();
    for c in pcm.chunks(4097) {
        chunked.push(c).unwrap();
    }
    let (a, b) = (whole.loudness(), chunked.loudness());
    assert!((a.lufs - b.lufs).abs() < 1e-6);
    assert!((a.peak - b.peak).abs() < 1e-9);
}

#[test]
fn silence_has_no_gain() {
    // 無音は積分ラウドネスが -inf（絶対ゲート以下）。gain は 0（補正なし）、peak は 0
    let mut m = LoudnessMeter::new(2, 44_100).unwrap();
    m.push(&vec![0.0f32; 44_100 * 2 * 3]).unwrap();
    let t = m.loudness();
    assert!(t.lufs.is_infinite());
    assert_eq!(t.gain(-18.0), 0.0);
    assert_eq!(t.peak, 0.0);
    // 直接構築した値でも同じ
    let t = TrackLoudness {
        lufs: f64::NEG_INFINITY,
        peak: 0.0,
    };
    assert_eq!(t.gain(-18.0), 0.0);
}

#[test]
fn album_loudness_is_gated_power_mean_of_members() {
    // -20 と -14 LUFS の同じ長さの 2 曲。相対ゲートには両方かかる（差 6 LU < 10 LU）ので
    // 電力平均: 10*log10((10^-2 + 10^-1.4) / 2) ≈ -16.04 LUFS
    let mut a = LoudnessMeter::new(2, 48_000).unwrap();
    a.push(&stereo_sine(dbfs(-20.0), 48_000, 4.0)).unwrap();
    let mut b = LoudnessMeter::new(2, 48_000).unwrap();
    b.push(&stereo_sine(dbfs(-14.0), 48_000, 4.0)).unwrap();
    let album = album_loudness(&[&a, &b]).unwrap();
    assert!((album - -16.04).abs() < 0.3, "album = {album}");
    // 1 曲だけなら track と同じ
    let solo = album_loudness(&[&a]).unwrap();
    assert!((solo - a.loudness().lufs).abs() < 1e-6);
}

#[test]
fn album_of_mixed_rates_is_accepted() {
    // 44.1k と 48k の曲が同じ album にあっても集計できる（状態ごとにレートを持つ）
    let mut a = LoudnessMeter::new(2, 44_100).unwrap();
    a.push(&stereo_sine(dbfs(-20.0), 44_100, 3.0)).unwrap();
    let mut b = LoudnessMeter::new(2, 48_000).unwrap();
    b.push(&stereo_sine(dbfs(-20.0), 48_000, 3.0)).unwrap();
    let album = album_loudness(&[&a, &b]).unwrap();
    assert!((album - -20.0).abs() < 0.2, "album = {album}");
}

#[test]
fn rejects_zero_channels() {
    assert!(LoudnessMeter::new(0, 44_100).is_err());
}

// ---------------------------------------------------------------- タグへの変換（P1-2）

mod write {
    use spindle::domain::replaygain::{
        file_matches, opus_r128, rg2_gain_db, tag_changes, Values, RG_KEYS,
    };
    use spindle::domain::tags::{normalize_tags, Codec, TagChange};

    const REFERENCE: f64 = -18.0;

    fn set(items: &[(&str, &str)]) -> spindle::domain::tags::TagSet {
        normalize_tags(items.iter().map(|(k, v)| (k.to_string(), v.to_string())))
    }

    fn change_of<'a>(changes: &'a [TagChange], key: &str) -> Option<&'a TagChange> {
        changes.iter().find(|c| c.key == key)
    }

    /// SPEC §6 のテストベクトル: 測定 -18 LUFS → G18 = 0 → R128 = -1280。測定 -23 LUFS → G18 = +5
    /// → R128 = 0。基準が 5 dB 低いので必ず減算する
    #[test]
    fn opus_r128_follows_spec_vectors() {
        assert_eq!(opus_r128(0.0, REFERENCE), -1280);
        assert_eq!(opus_r128(5.0, REFERENCE), 0);
        // 測定 -13 LUFS → G18 = -5 → (-5 - 5) * 256 = -2560
        assert_eq!(opus_r128(-5.0, REFERENCE), -2560);
        // 四捨五入（0.5 は 0 から遠い方へ）
        assert_eq!(opus_r128(5.0 + 0.5 / 256.0, REFERENCE), 1);
        assert_eq!(opus_r128(5.0 - 0.5 / 256.0, REFERENCE), -1);
        // 内部基準が -23 なら減算は 0
        assert_eq!(opus_r128(0.0, -23.0), 0);
    }

    #[test]
    fn opus_r128_saturates_to_i16() {
        assert_eq!(opus_r128(200.0, REFERENCE), i16::MAX);
        assert_eq!(opus_r128(-200.0, REFERENCE), i16::MIN);
        assert_eq!(opus_r128(f64::NAN, REFERENCE), 0);
    }

    #[test]
    fn rg2_gain_is_rebased_to_minus_18() {
        assert_eq!(rg2_gain_db(0.0, -18.0), 0.0);
        assert_eq!(rg2_gain_db(0.0, -23.0), 5.0);
    }

    #[test]
    fn opus_changes_are_r128_only_and_drop_replaygain_keys() {
        let v = Values {
            track_gain: 0.0,
            track_peak: 0.5,
            album_gain: Some(5.0),
            album_peak: Some(0.9),
        };
        let changes = tag_changes(Codec::Opus, &v, REFERENCE);
        assert_eq!(
            change_of(&changes, "R128_TRACK_GAIN").unwrap().values,
            Some(vec!["-1280".to_owned()])
        );
        assert_eq!(
            change_of(&changes, "R128_ALBUM_GAIN").unwrap().values,
            Some(vec!["0".to_owned()])
        );
        for key in [
            "REPLAYGAIN_TRACK_GAIN",
            "REPLAYGAIN_TRACK_PEAK",
            "REPLAYGAIN_ALBUM_GAIN",
            "REPLAYGAIN_ALBUM_PEAK",
        ] {
            assert_eq!(change_of(&changes, key).unwrap().values, None, "{key}");
        }
        // Opus に peak は無い
        assert!(change_of(&changes, "R128_TRACK_PEAK").is_none());
    }

    #[test]
    fn opus_without_album_removes_album_gain() {
        let v = Values {
            track_gain: 1.0,
            track_peak: 0.5,
            album_gain: None,
            album_peak: None,
        };
        let changes = tag_changes(Codec::Opus, &v, REFERENCE);
        assert_eq!(change_of(&changes, "R128_ALBUM_GAIN").unwrap().values, None);
    }

    #[test]
    fn vorbis_comment_changes_are_rg2_strings() {
        let v = Values {
            track_gain: -7.346,
            track_peak: 0.987654321,
            album_gain: Some(0.0),
            album_peak: Some(1.05),
        };
        for codec in [Codec::Flac, Codec::Ogg, Codec::Alac, Codec::Aac, Codec::Mp3] {
            let changes = tag_changes(codec, &v, REFERENCE);
            assert_eq!(
                change_of(&changes, "REPLAYGAIN_TRACK_GAIN").unwrap().values,
                Some(vec!["-7.35 dB".to_owned()]),
                "{codec:?}"
            );
            assert_eq!(
                change_of(&changes, "REPLAYGAIN_TRACK_PEAK").unwrap().values,
                Some(vec!["0.987654".to_owned()])
            );
            assert_eq!(
                change_of(&changes, "REPLAYGAIN_ALBUM_GAIN").unwrap().values,
                Some(vec!["+0.00 dB".to_owned()])
            );
            assert_eq!(
                change_of(&changes, "REPLAYGAIN_ALBUM_PEAK").unwrap().values,
                Some(vec!["1.050000".to_owned()])
            );
            // R128_* は Opus 専用なので触らない（generic Tag には写像できない）
            assert!(
                change_of(&changes, "R128_TRACK_GAIN").is_none(),
                "{codec:?}"
            );
        }
    }

    #[test]
    fn vorbis_comment_without_album_removes_album_keys() {
        let v = Values {
            track_gain: -0.0,
            track_peak: 0.0,
            album_gain: None,
            album_peak: None,
        };
        let changes = tag_changes(Codec::Flac, &v, REFERENCE);
        // -0.0 は "+0.00 dB"
        assert_eq!(
            change_of(&changes, "REPLAYGAIN_TRACK_GAIN").unwrap().values,
            Some(vec!["+0.00 dB".to_owned()])
        );
        assert_eq!(
            change_of(&changes, "REPLAYGAIN_ALBUM_GAIN").unwrap().values,
            None
        );
        assert_eq!(
            change_of(&changes, "REPLAYGAIN_ALBUM_PEAK").unwrap().values,
            None
        );
    }

    #[test]
    fn rg2_gain_is_rebased_in_tag_changes() {
        let v = Values {
            track_gain: 0.0,
            track_peak: 0.5,
            album_gain: None,
            album_peak: None,
        };
        let changes = tag_changes(Codec::Flac, &v, -23.0);
        assert_eq!(
            change_of(&changes, "REPLAYGAIN_TRACK_GAIN").unwrap().values,
            Some(vec!["+5.00 dB".to_owned()])
        );
        let changes = tag_changes(Codec::Opus, &v, -23.0);
        assert_eq!(
            change_of(&changes, "R128_TRACK_GAIN").unwrap().values,
            Some(vec!["0".to_owned()])
        );
    }

    #[test]
    fn every_change_key_is_a_known_rg_key() {
        let v = Values {
            track_gain: 1.0,
            track_peak: 0.5,
            album_gain: Some(1.0),
            album_peak: Some(0.5),
        };
        for codec in [Codec::Flac, Codec::Opus, Codec::Mp3] {
            for c in tag_changes(codec, &v, REFERENCE) {
                assert!(RG_KEYS.contains(&c.key.as_str()), "{codec:?} {}", c.key);
            }
        }
    }

    #[test]
    fn file_matches_when_all_keys_equal_and_removed_keys_absent() {
        let v = Values {
            track_gain: -7.346,
            track_peak: 0.987654321,
            album_gain: None,
            album_peak: None,
        };
        let ok = set(&[
            ("TITLE", "x"),
            ("REPLAYGAIN_TRACK_GAIN", "-7.35 dB"),
            ("REPLAYGAIN_TRACK_PEAK", "0.987654"),
        ]);
        assert!(file_matches(Codec::Flac, &ok, &v, REFERENCE));
        // album のキーが残っている
        let stale_album = set(&[
            ("REPLAYGAIN_TRACK_GAIN", "-7.35 dB"),
            ("REPLAYGAIN_TRACK_PEAK", "0.987654"),
            ("REPLAYGAIN_ALBUM_GAIN", "+1.00 dB"),
        ]);
        assert!(!file_matches(Codec::Flac, &stale_album, &v, REFERENCE));
        // 表記が違う（外部ツールの書式）は不一致
        let other_format = set(&[
            ("REPLAYGAIN_TRACK_GAIN", "-7.350000 dB"),
            ("REPLAYGAIN_TRACK_PEAK", "0.987654"),
        ]);
        assert!(!file_matches(Codec::Flac, &other_format, &v, REFERENCE));
        // 多値は不一致
        let multi = set(&[
            ("REPLAYGAIN_TRACK_GAIN", "-7.35 dB"),
            ("REPLAYGAIN_TRACK_GAIN", "-7.35 dB"),
            ("REPLAYGAIN_TRACK_PEAK", "0.987654"),
        ]);
        assert!(!file_matches(Codec::Flac, &multi, &v, REFERENCE));
        assert!(!file_matches(Codec::Flac, &set(&[]), &v, REFERENCE));
        // Opus は R128 だけを見て、REPLAYGAIN_* が残っていれば不一致
        let opus_ok = set(&[("R128_TRACK_GAIN", &opus_r128(-7.346, REFERENCE).to_string())]);
        assert!(file_matches(Codec::Opus, &opus_ok, &v, REFERENCE));
        let opus_stale = set(&[
            ("R128_TRACK_GAIN", &opus_r128(-7.346, REFERENCE).to_string()),
            ("REPLAYGAIN_TRACK_GAIN", "-7.35 dB"),
        ]);
        assert!(!file_matches(Codec::Opus, &opus_stale, &v, REFERENCE));
    }
}
