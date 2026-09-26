//! 音声フィンガープリント（SPEC §6「変更検出と版の遷移」、D-30）。
//! FLAC は STREAMINFO の MD5、ALAC / WAV はデコードした PCM の MD5、非可逆はパケット列の SHA-256。
//! 受け入れ: docs/TASKS.md P0-5 (c) (l)
//!
//! 圧縮形式のファイルは ffmpeg で作る。ffmpeg が無い環境ではそのテストを skip する
//! （CI には ffmpeg を入れる）。

use std::fs::File;
use std::io::Cursor;

mod common;

use md5::{Digest, Md5};

use spindle::media::fingerprint::{
    decoded_pcm_md5, flac_streaminfo_md5, flac_streaminfo_md5_at, packet_fp, FingerprintError,
    FrameLimit,
};

// ---------------------------------------------------------------- 生成ヘルパ

use common::{
    encode, ffmpeg_frames, pack_pcm, pcm_samples, set_edit, write_wav, zero_last_stts_delta,
    EditEnd,
};

/// 手組みの最小 FLAC（fLaC マーカー + STREAMINFO のみ）。`md5` を埋める
fn minimal_flac(md5: [u8; 16]) -> Vec<u8> {
    let mut v = b"fLaC".to_vec();
    v.push(0x80); // last-metadata-block, type 0 = STREAMINFO
    v.extend_from_slice(&[0, 0, 34]);
    let mut info = [0u8; 34];
    // min / max blocksize
    info[0..2].copy_from_slice(&4096u16.to_be_bytes());
    info[2..4].copy_from_slice(&4096u16.to_be_bytes());
    // sample rate 44100 (20bit) | channels-1 (3bit) | bps-1 (5bit) | total samples (36bit)
    let packed: u64 = (44_100u64 << 44) | (1u64 << 41) | (15u64 << 36) | 44_100u64;
    info[10..18].copy_from_slice(&packed.to_be_bytes());
    info[18..34].copy_from_slice(&md5);
    v.extend_from_slice(&info);
    v
}

fn md5_of(bytes: &[u8]) -> [u8; 16] {
    Md5::digest(bytes).into()
}

// ---------------------------------------------------------------- FLAC STREAMINFO

#[test]
fn flac_md5_is_read_from_streaminfo_without_decoding() {
    let md5 = [0x11u8; 16];
    let flac = minimal_flac(md5);
    assert_eq!(flac_streaminfo_md5(Cursor::new(flac)).unwrap(), Some(md5));
}

#[test]
fn flac_without_md5_is_none_not_an_error() {
    // 受け入れ (c): MD5 未設定（全ゼロ）を「MD5 なし」として扱い、落ちない
    let flac = minimal_flac([0u8; 16]);
    assert_eq!(flac_streaminfo_md5(Cursor::new(flac)).unwrap(), None);
}

#[test]
fn flac_md5_skips_preceding_metadata_blocks() {
    // STREAMINFO は常に先頭だが、fLaC の前に ID3v2 が付いた FLAC は存在する
    let md5 = [0x22u8; 16];
    let mut v = b"ID3\x04\x00\x00\x00\x00\x00\x0a".to_vec(); // 10 バイトのボディ
    v.extend_from_slice(&[0u8; 10]);
    v.extend_from_slice(&minimal_flac(md5));
    assert_eq!(flac_streaminfo_md5(Cursor::new(v)).unwrap(), Some(md5));
}

#[test]
fn flac_md5_offset_points_at_the_16_bytes_in_streaminfo() {
    // MD5 補填（P1-5b）はこの位置の 16 バイトだけを書き換える。ID3v2 が前置されていれば
    // その分ずれる
    let md5 = [0x33u8; 16];
    let flac = minimal_flac(md5);
    let (offset, current) = flac_streaminfo_md5_at(Cursor::new(&flac)).unwrap();
    assert_eq!(offset, 4 + 4 + 18);
    assert_eq!(current, md5);
    assert_eq!(&flac[offset as usize..offset as usize + 16], &md5);

    let mut v = b"ID3\x04\x00\x00\x00\x00\x00\x0a".to_vec();
    v.extend_from_slice(&[0u8; 10]);
    v.extend_from_slice(&minimal_flac([0u8; 16]));
    let (offset, current) = flac_streaminfo_md5_at(Cursor::new(&v)).unwrap();
    assert_eq!(offset, 20 + 26);
    assert_eq!(current, [0u8; 16], "全ゼロもそのまま返す（None にしない）");
}

#[test]
fn non_flac_and_truncated_input_are_errors() {
    assert!(matches!(
        flac_streaminfo_md5(Cursor::new(b"RIFF....WAVE".to_vec())),
        Err(FingerprintError::NotFlac)
    ));
    let mut short = minimal_flac([1u8; 16]);
    short.truncate(20);
    assert!(flac_streaminfo_md5(Cursor::new(short)).is_err());
}

// ---------------------------------------------------------------- WAV / ALAC → PCM MD5

#[test]
fn wav_pcm_md5_matches_raw_sample_bytes_16bit() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    let samples = pcm_samples(0);
    write_wav(&wav, &samples, 16);
    let got = decoded_pcm_md5(File::open(&wav).unwrap(), Some("wav")).unwrap();
    assert_eq!(got, md5_of(&pack_pcm(&samples, 16)));
}

#[test]
fn wav_pcm_md5_matches_raw_sample_bytes_24bit() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    let samples: Vec<i32> = pcm_samples(0).iter().map(|s| s * 200).collect();
    write_wav(&wav, &samples, 24);
    let got = decoded_pcm_md5(File::open(&wav).unwrap(), Some("wav")).unwrap();
    assert_eq!(got, md5_of(&pack_pcm(&samples, 24)));
}

#[test]
fn flac_alac_and_wav_of_same_pcm_share_audio_md5() {
    // WAV → FLAC 正規化や ALAC からの移行で audio_md5 が変わらないための性質
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    let samples = pcm_samples(0);
    write_wav(&wav, &samples, 16);
    let expected = md5_of(&pack_pcm(&samples, 16));

    let flac = dir.path().join("a.flac");
    require_ffmpeg!(encode(&wav, &flac, &["-c:a", "flac"]));
    assert_eq!(
        flac_streaminfo_md5(File::open(&flac).unwrap()).unwrap(),
        Some(expected)
    );

    let alac = dir.path().join("a.m4a");
    encode(&wav, &alac, &["-c:a", "alac"]).unwrap();
    assert_eq!(
        decoded_pcm_md5(File::open(&alac).unwrap(), Some("m4a")).unwrap(),
        expected
    );
}

#[test]
fn alac_24bit_shares_audio_md5_with_flac() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    let samples: Vec<i32> = pcm_samples(0).iter().map(|s| s * 200).collect();
    write_wav(&wav, &samples, 24);
    let flac = dir.path().join("a.flac");
    require_ffmpeg!(encode(&wav, &flac, &["-c:a", "flac", "-sample_fmt", "s32"]));
    let alac = dir.path().join("a.m4a");
    encode(&wav, &alac, &["-c:a", "alac", "-sample_fmt", "s32p"]).unwrap();
    let from_flac = flac_streaminfo_md5(File::open(&flac).unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(from_flac, md5_of(&pack_pcm(&samples, 24)));
    assert_eq!(
        decoded_pcm_md5(File::open(&alac).unwrap(), Some("m4a")).unwrap(),
        from_flac
    );
}

/// 24 bit の ALAC を作り、末尾サンプルを長さ 0 と宣言し直す。(サンプル, 宣言から外したフレーム数, パス)
fn alac_with_zero_duration_tail(
    dir: &std::path::Path,
    edit: EditEnd,
) -> Option<(Vec<i32>, usize, std::path::PathBuf)> {
    let wav = dir.join("a.wav");
    let samples: Vec<i32> = pcm_samples(0).iter().map(|s| s * 200).collect();
    write_wav(&wav, &samples, 24);
    let alac = dir.join("a.m4a");
    encode(&wav, &alac, &["-c:a", "alac", "-sample_fmt", "s32p"])?;
    let dropped = zero_last_stts_delta(&alac, edit) as usize;
    assert!(dropped > 0 && dropped < samples.len() / 2);
    Some((samples, dropped, alac))
}

#[test]
fn alac_zero_duration_tail_outside_the_edit_list_is_not_hashed() {
    // D-89: 実機の ALAC は stts の末尾に長さ 0 のサンプルを持つ（宣言長の外）。edit list も宣言長で
    // 終わっていれば ffmpeg はそれを捨てる。symphonia 0.6.1 はデコードして返すので、宣言長で打ち切らないと
    // PCM MD5 が食い違い、正規化の照合が失敗する
    let dir = tempfile::tempdir().unwrap();
    let (samples, dropped, alac) = require_ffmpeg!(alac_with_zero_duration_tail(
        dir.path(),
        EditEnd::AtDeclared
    ));
    let frames = samples.len() / 2;
    assert_eq!(ffmpeg_frames(&alac), Some(frames - dropped));
    let kept = &samples[..(frames - dropped) * 2];
    assert_eq!(
        decoded_pcm_md5(File::open(&alac).unwrap(), Some("m4a")).unwrap(),
        md5_of(&pack_pcm(kept, 24))
    );
}

#[test]
fn alac_zero_duration_tail_inside_the_edit_list_is_hashed() {
    // D-89 追記: edit list が宣言長より後ろで終わる（または無い）と、ffmpeg は長さ 0 のサンプルも
    // デコードする（2026-09-26 の再移行で 46 本。うち 3 本は末尾にフェードの実音声があった）。打ち切ると
    // 正規化の照合が失敗するうえ本物の末尾を落とすので、全部を MD5 に入れる
    for edit in [EditEnd::Beyond, EditEnd::Removed] {
        let dir = tempfile::tempdir().unwrap();
        let (samples, _, alac) = require_ffmpeg!(alac_with_zero_duration_tail(dir.path(), edit));
        assert_eq!(ffmpeg_frames(&alac), Some(samples.len() / 2), "{edit:?}");
        assert_eq!(
            decoded_pcm_md5(File::open(&alac).unwrap(), Some("m4a")).unwrap(),
            md5_of(&pack_pcm(&samples, 24)),
            "{edit:?}"
        );
    }
}

#[test]
fn frame_limit_skips_the_head_and_stops_at_the_end() {
    let mut l = FrameLimit::new(5, Some(100));
    assert_eq!(l.take(4), 4..4);
    assert_eq!(l.take(4), 1..4);
    assert_eq!(l.take(4), 0..4);
    assert!(!l.stops_at(99));
    assert!(l.stops_at(100));
    let mut none = FrameLimit::NONE;
    assert_eq!(none.take(4096), 0..4096);
    assert!(!none.stops_at(i64::MAX));
}

/// ffmpeg がこのファイルから作る FLAC の STREAMINFO MD5（= 正規化の照合相手）。`ignore_editlist` なら
/// edit list を無視して全パケットをデコードした値
fn ffmpeg_flac_md5(src: &std::path::Path, ignore_editlist: bool) -> Option<[u8; 16]> {
    let flac = src.with_extension("check.flac");
    let input_opts: &[&str] = if ignore_editlist {
        &["-ignore_editlist", "1"]
    } else {
        &[]
    };
    let out = std::process::Command::new(common::ffmpeg()?)
        .args(["-v", "error", "-y"])
        .args(input_opts)
        .arg("-i")
        .arg(src)
        .args(["-map", "0:a", "-c:a", "flac"])
        .arg(&flac)
        .output()
        .ok()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let md5 = flac_streaminfo_md5(File::open(&flac).unwrap()).unwrap();
    let _ = std::fs::remove_file(&flac);
    md5
}

/// 24 bit の ALAC（1 秒、44.1k、4096 フレームのパケット 11 個 = 最後は 3140）
fn alac_24bit(dir: &std::path::Path) -> Option<(Vec<i32>, std::path::PathBuf)> {
    let wav = dir.join("src.wav");
    let samples: Vec<i32> = pcm_samples(0).iter().map(|s| s * 200).collect();
    write_wav(&wav, &samples, 24);
    let alac = dir.join("src.m4a");
    encode(&wav, &alac, &["-c:a", "alac", "-sample_fmt", "s32p"])?;
    Some((samples, alac))
}

#[test]
fn alac_edit_lists_written_by_the_muxer_match_ffmpeg() {
    // D-89 追記 2: ffmpeg の stream copy で途中から・途中までを切り出すと、muxer が elst を書く。
    // 入力側の -ss は先頭を削る media_time と区間、-t は区間の長さ。どちらも終端 = 全パケットの終わりで、
    // どの版の ffmpeg でも同じになるので、手元の ffmpeg で作った FLAC の MD5（= 正規化の照合相手）と比べる
    let dir = tempfile::tempdir().unwrap();
    let (_, alac) = require_ffmpeg!(alac_24bit(dir.path()));
    for (name, input_ss, t) in [
        ("t", None, Some("0.5432")),
        ("ss_t", Some("0.1234"), Some("0.5432")),
        ("ss", Some("0.1234"), None),
    ] {
        let cut = dir.path().join(format!("{name}.m4a"));
        let mut cmd = std::process::Command::new(common::ffmpeg().unwrap());
        cmd.args(["-v", "error", "-y"]);
        if let Some(ss) = input_ss {
            cmd.args(["-ss", ss]);
        }
        cmd.arg("-i").arg(&alac).args(["-map", "0:a", "-c", "copy"]);
        if let Some(t) = t {
            cmd.args(["-t", t]);
        }
        assert!(cmd.arg(&cut).status().unwrap().success());
        assert_eq!(
            Some(decoded_pcm_md5(File::open(&cut).unwrap(), Some("m4a")).unwrap()),
            ffmpeg_flac_md5(&cut, false),
            "{name}"
        );
    }
}

#[test]
fn alac_uses_its_own_edit_list_when_another_audio_track_comes_first() {
    // codex の指摘: symphonia は codec の分かる最初の音声トラックを選ぶ。先頭の AC-3 は encoder delay の elst
    // （media_time 256）を持つ。そのサンプルエントリを未知の種別に書き換えると symphonia は 2 本目の ALAC を選ぶので、
    // 先頭トラックの edit list を ALAC に当てると先頭を誤って削る
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    write_wav(&wav, &pcm_samples(0), 16);
    let two = dir.path().join("two.m4a");
    let Some(ffmpeg) = common::ffmpeg() else {
        eprintln!("ffmpeg が無いので skip");
        return;
    };
    let ok = std::process::Command::new(&ffmpeg)
        .args(["-v", "error", "-y", "-i"])
        .arg(&wav)
        .arg("-i")
        .arg(&wav)
        .args([
            "-map", "0:a", "-map", "1:a", "-c:a:0", "ac3", "-c:a:1", "alac",
        ])
        .arg(&two)
        .status()
        .unwrap()
        .success();
    if !ok {
        eprintln!("ffmpeg に AC-3 エンコーダが無いので skip");
        return;
    }
    let alone = dir.path().join("alone.m4a");
    require_ffmpeg!(encode(&two, &alone, &["-map", "0:a:1", "-c", "copy"]));
    let mut data = std::fs::read(&two).unwrap();
    let at = data
        .windows(4)
        .position(|w| w == b"ac-3")
        .expect("AC-3 のサンプルエントリが無い");
    data[at..at + 4].copy_from_slice(b"zzzz");
    std::fs::write(&two, data).unwrap();
    let edits = spindle::media::mp4edit::read_audio_edits(&mut File::open(&two).unwrap());
    assert_eq!(edits.len(), 2, "{edits:?}");
    assert!(edits[0].entries[0].media_time > 0, "{edits:?}");
    let md5 = decoded_pcm_md5(File::open(&alone).unwrap(), Some("m4a")).unwrap();
    assert_eq!(Some(md5), ffmpeg_flac_md5(&alone, false));
    assert_eq!(
        decoded_pcm_md5(File::open(&two).unwrap(), Some("m4a")).unwrap(),
        md5
    );
}

#[test]
fn alac_empty_edit_before_the_segment_is_not_reproduced() {
    // 出力側の -ss で切り出すと、muxer は「空の編集 + 区間」の 2 項を書く。本番の ffmpeg 5.1 は全パケットを
    // デコードし（2026-09-26 に確認）、新しい ffmpeg は末尾を空の編集の長さだけ削る。単純な形でない edit list は
    // 再現せず全パケットを出す = 5.1 と同じ。手元の ffmpeg の版に依らないよう、edit list を無視した値と比べる
    let dir = tempfile::tempdir().unwrap();
    let (_, alac) = require_ffmpeg!(alac_24bit(dir.path()));
    let cut = dir.path().join("out_ss.m4a");
    require_ffmpeg!(encode(
        &alac,
        &cut,
        &["-map", "0:a", "-c", "copy", "-ss", "0.1234"]
    ));
    let edit = spindle::media::mp4edit::read_audio_edits(&mut File::open(&cut).unwrap())
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(edit.edit_count, 2, "{edit:?}");
    assert_eq!(edit.entries[0].media_time, -1, "{edit:?}");
    assert_eq!(
        Some(decoded_pcm_md5(File::open(&cut).unwrap(), Some("m4a")).unwrap()),
        ffmpeg_flac_md5(&cut, true)
    );
}

#[test]
fn alac_head_is_trimmed_by_media_time() {
    // 先頭を削る編集はどの版の ffmpeg も media_time ちょうど削る
    let dir = tempfile::tempdir().unwrap();
    let (samples, alac) = require_ffmpeg!(alac_24bit(dir.path()));
    set_edit(&alac, 44_100, 44_100 - 1000, 1000);
    assert_eq!(ffmpeg_frames(&alac), Some(44_100 - 1000));
    assert_eq!(
        decoded_pcm_md5(File::open(&alac).unwrap(), Some("m4a")).unwrap(),
        md5_of(&pack_pcm(&samples[1000 * 2..], 24))
    );
}

#[test]
fn alac_packet_straddling_the_edit_end_is_kept_whole() {
    // 本番の ffmpeg 5.1 の規則（D-89 追記 2）: 終端以降で始まるパケットは捨て、終端をまたぐパケットは丸ごと残す。
    // 新しい ffmpeg は終端で端数を切るので、ここは手元の ffmpeg と比べず 5.1 で確かめた値に固定する。
    // 最後のパケットは 40,960 から始まる
    let dir = tempfile::tempdir().unwrap();
    let (samples, alac) = require_ffmpeg!(alac_24bit(dir.path()));
    let md5_upto = |frames: usize| md5_of(&pack_pcm(&samples[..frames * 2], 24));
    for (movie_ts, seg, frames) in [
        // 終端が最後のパケットの途中 → 丸ごと残す（5.1 は 44,100。新しい ffmpeg は 42,000）
        (44_100, 42_000, 44_100),
        // 終端が最後のパケットの手前 → 最後のパケットを捨て、その前のパケットは丸ごと（5.1 は 40,960）
        (44_100, 40_000, 40_960),
        // 終端 = 40,960 + 0.4 サンプル → 四捨五入で 40,960 = 最後のパケットの開始 → 捨てる
        (441_000, 409_604, 40_960),
        // 終端 = 40,960 + 0.5 サンプル → 40,961 → 残す
        (441_000, 409_605, 44_100),
    ] {
        set_edit(&alac, movie_ts, seg, 0);
        assert_eq!(
            decoded_pcm_md5(File::open(&alac).unwrap(), Some("m4a")).unwrap(),
            md5_upto(frames),
            "movie_ts={movie_ts} seg={seg}"
        );
    }
}

// ---------------------------------------------------------------- 非可逆のパケット列

fn assert_packet_fp_survives_retag(ext: &str, args: &[&str]) {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    write_wav(&wav, &pcm_samples(0), 16);
    let dst = dir.path().join(format!("a.{ext}"));
    require_ffmpeg!(encode(&wav, &dst, args));

    let before = packet_fp(File::open(&dst).unwrap(), Some(ext)).unwrap();
    let size_before = std::fs::metadata(&dst).unwrap().len();
    common::retag(&dst, |tag| {
        use lofty::tag::{Accessor, ItemKey};
        tag.set_title("書き換え後のタイトル".to_owned());
        tag.insert_text(ItemKey::Comment, "x".repeat(4096));
    });
    let size_after = std::fs::metadata(&dst).unwrap().len();
    assert_ne!(
        size_before, size_after,
        "タグ書き換えでコンテナサイズが変わる前提"
    );
    let after = packet_fp(File::open(&dst).unwrap(), Some(ext)).unwrap();
    assert_eq!(
        before, after,
        "{ext}: タグだけの変更で audio_fp が変わってはいけない"
    );

    // 別の音声なら別の値
    let wav2 = dir.path().join("b.wav");
    write_wav(&wav2, &pcm_samples(7), 16);
    let dst2 = dir.path().join(format!("b.{ext}"));
    encode(&wav2, &dst2, args).unwrap();
    assert_ne!(
        packet_fp(File::open(&dst2).unwrap(), Some(ext)).unwrap(),
        before
    );
}

#[test]
fn mp3_packet_fp_survives_tag_rewrite() {
    assert_packet_fp_survives_retag("mp3", &["-c:a", "libmp3lame", "-b:a", "128k"]);
}

#[test]
fn opus_packet_fp_survives_tag_rewrite() {
    assert_packet_fp_survives_retag("opus", &["-c:a", "libopus", "-b:a", "96k"]);
}

#[test]
fn aac_in_mp4_packet_fp_survives_tag_rewrite() {
    assert_packet_fp_survives_retag("m4a", &["-c:a", "aac", "-b:a", "128k"]);
}

#[test]
fn ogg_vorbis_packet_fp_survives_tag_rewrite() {
    assert_packet_fp_survives_retag("ogg", &["-c:a", "libvorbis", "-q:a", "3"]);
}
