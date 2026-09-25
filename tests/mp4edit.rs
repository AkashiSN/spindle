//! MP4 の音声トラックの edit list の読み取り（D-89 追記、`media::mp4edit`）

use std::io::Cursor;

use spindle::media::mp4edit::{read_audio_edit, AudioEdit, EditEntry, EditWindow};

fn boxed(typ: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut v = ((body.len() + 8) as u32).to_be_bytes().to_vec();
    v.extend_from_slice(typ);
    v.extend_from_slice(body);
    v
}

/// version 0 の mvhd / mdhd（timescale と duration だけ意味を持つ）
fn time_header(typ: &[u8; 4], timescale: u32) -> Vec<u8> {
    let mut b = vec![0u8; 12];
    b.extend_from_slice(&timescale.to_be_bytes());
    b.extend_from_slice(&0u32.to_be_bytes());
    b.extend_from_slice(&[0u8; 8]);
    boxed(typ, &b)
}

fn hdlr(kind: &[u8; 4]) -> Vec<u8> {
    let mut b = vec![0u8; 8];
    b.extend_from_slice(kind);
    b.extend_from_slice(&[0u8; 12]);
    boxed(b"hdlr", &b)
}

fn elst_v0(entries: &[(u32, i32)]) -> Vec<u8> {
    let mut b = vec![0u8; 4];
    b.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (seg, media_time) in entries {
        b.extend_from_slice(&seg.to_be_bytes());
        b.extend_from_slice(&media_time.to_be_bytes());
        b.extend_from_slice(&[0, 1, 0, 0]);
    }
    boxed(b"elst", &b)
}

fn trak(kind: &[u8; 4], media_ts: u32, elst: Option<Vec<u8>>) -> Vec<u8> {
    let mdia = boxed(
        b"mdia",
        &[time_header(b"mdhd", media_ts), hdlr(kind)].concat(),
    );
    let mut body = Vec::new();
    if let Some(elst) = elst {
        body.extend(boxed(b"edts", &elst));
    }
    body.extend(mdia);
    boxed(b"trak", &body)
}

/// ftyp → mdat → moov（moov が後ろにある形）
fn mp4(movie_ts: u32, traks: &[Vec<u8>]) -> Vec<u8> {
    let moov = boxed(
        b"moov",
        &[vec![time_header(b"mvhd", movie_ts)], traks.to_vec()]
            .concat()
            .concat(),
    );
    [
        boxed(b"ftyp", b"M4A \0\0\0\0"),
        boxed(b"mdat", &[0xAA; 64]),
        moov,
    ]
    .concat()
}

fn read(data: Vec<u8>) -> Option<AudioEdit> {
    read_audio_edit(&mut Cursor::new(data))
}

#[test]
fn reads_the_first_audio_track_after_mdat() {
    let data = mp4(
        1000,
        &[
            trak(b"vide", 90_000, Some(elst_v0(&[(5, 0)]))),
            trak(b"soun", 48_000, Some(elst_v0(&[(310_870, 0)]))),
        ],
    );
    let e = read(data).unwrap();
    assert_eq!(e.movie_timescale, 1000);
    assert_eq!(e.media_timescale, 48_000);
    assert_eq!(
        e.entries,
        vec![EditEntry {
            segment_duration: 310_870,
            media_time: 0,
            rate: (1, 0)
        }]
    );
}

#[test]
fn not_mp4_or_no_audio_track_is_none() {
    assert_eq!(read(b"fLaC\0\0\0\x22 rest of a flac file".to_vec()), None);
    assert_eq!(read(Vec::new()), None);
    assert_eq!(read(mp4(1000, &[trak(b"vide", 90_000, None)])), None);
    // サイズが親を超える壊れた箱
    let mut data = mp4(1000, &[trak(b"soun", 44_100, None)]);
    let n = data.len();
    data.truncate(n - 10);
    assert_eq!(read(data), None);
}

#[test]
fn missing_edit_list_is_empty() {
    let e = read(mp4(1000, &[trak(b"soun", 44_100, None)])).unwrap();
    assert!(e.entries.is_empty());
    assert_eq!(e.window(), None);
}

fn plain(movie_timescale: u32, media_timescale: u32, seg: u64, media_time: i64) -> AudioEdit {
    AudioEdit {
        movie_timescale,
        media_timescale,
        entries: vec![EditEntry {
            segment_duration: seg,
            media_time,
            rate: (1, 0),
        }],
    }
}

#[test]
fn window_converts_the_segment_to_media_timescale() {
    // 実機の 46 本: 310,870 ms = 14,921,760 > 宣言長 14,921,728（長さ 0 の末尾サンプルは終端より前で始まる）
    assert_eq!(
        plain(1000, 48_000, 310_870, 0).window(),
        Some(EditWindow {
            start: 0,
            end: 14_921_760
        })
    );
    // D-89 の 7 本: 156,416 ms = 7,507,968 = 宣言長（長さ 0 の末尾サンプルは終端で始まる = 捨てる）
    assert_eq!(
        plain(1000, 48_000, 156_416, 0).window().map(|w| w.end),
        Some(7_507_968)
    );
    // 先頭を削る編集（ffmpeg の stream copy が書く形）。終端は media_time + 区間
    assert_eq!(
        plain(48_000, 48_000, 262_706, 3534).window(),
        Some(EditWindow {
            start: 3534,
            end: 266_240
        })
    );
}

#[test]
fn window_rounds_like_ffmpeg() {
    // ffmpeg 5.1 で確かめた境界: 終端 = 237,568 + 0.4 サンプルは 237,568（そこで始まるパケットを捨てる）、
    // + 0.5 は 237,569（残す）。44.1k で 1/1000 秒の区間は 5475 ms = 241,447.5 → 241,448
    assert_eq!(
        plain(441_000, 44_100, 2_375_684, 0).window().map(|w| w.end),
        Some(237_568)
    );
    assert_eq!(
        plain(441_000, 44_100, 2_375_685, 0).window().map(|w| w.end),
        Some(237_569)
    );
    assert_eq!(
        plain(1000, 44_100, 5475, 0).window().map(|w| w.end),
        Some(241_448)
    );
    assert_eq!(
        plain(1000, 44_100, 5479, 0).window().map(|w| w.end),
        Some(241_624)
    );
    assert_eq!(
        plain(1000, 44_100, 5445, 1000).window().map(|w| w.end),
        Some(241_125)
    );
}

#[test]
fn only_a_single_plain_edit_has_a_window() {
    let ok = plain(1000, 1000, 10, 0);
    assert!(ok.window().is_some());
    let entry = ok.entries[0];
    let with = |entries: Vec<EditEntry>| AudioEdit {
        entries,
        ..ok.clone()
    };
    // 空の編集・速度違い・長さ 0 の区間・複数区間は再現しない（何も削らない）
    for e in [
        EditEntry {
            media_time: -1,
            ..entry
        },
        EditEntry {
            rate: (2, 0),
            ..entry
        },
        EditEntry {
            segment_duration: 0,
            ..entry
        },
    ] {
        assert_eq!(with(vec![e]).window(), None, "{e:?}");
    }
    assert_eq!(with(vec![entry, entry]).window(), None);
    let zero_movie = AudioEdit {
        movie_timescale: 0,
        ..ok.clone()
    };
    assert_eq!(zero_movie.window(), None);
}

#[test]
fn reads_version_1_elst() {
    let mut b = vec![1u8, 0, 0, 0];
    b.extend_from_slice(&1u32.to_be_bytes());
    b.extend_from_slice(&7_507_968u64.to_be_bytes());
    b.extend_from_slice(&0u64.to_be_bytes());
    b.extend_from_slice(&[0, 1, 0, 0]);
    let data = mp4(48_000, &[trak(b"soun", 48_000, Some(boxed(b"elst", &b)))]);
    let e = read(data).unwrap();
    assert_eq!(e.entries[0].segment_duration, 7_507_968);
    assert_eq!(e.window().map(|w| w.end), Some(7_507_968));
}
