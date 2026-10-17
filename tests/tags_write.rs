//! lofty ラッパによるタグ書き換え（SPEC §7.5、docs/TASKS.md P0-9）。
//! 指定キーだけを置き換え・削除し、他のキー（任意キー・多値・画像）は保つ。

mod common;

use std::fs::File;
use std::path::Path;

use spindle::domain::tags::{read_audio_file, write_tag_changes, TagChange};

fn values(path: &Path, ext: &str, key: &str) -> Vec<String> {
    let af = read_audio_file(File::open(path).unwrap(), Some(ext)).unwrap();
    af.tags.values(key).map(str::to_owned).collect()
}

fn change(key: &str, values: Option<&[&str]>) -> TagChange {
    TagChange {
        key: key.to_owned(),
        values: values.map(|v| v.iter().map(|s| (*s).to_owned()).collect()),
    }
}

fn write(path: &Path, ext: &str, changes: &[TagChange]) {
    let mut f = File::options().read(true).write(true).open(path).unwrap();
    write_tag_changes(&mut f, Some(ext), changes, None).unwrap();
}

#[test]
fn flac_replaces_deletes_and_keeps_other_keys() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.flac", "flac", 0));
    common::set_basic_tags(&p, "t", "art", "alb", "aa", 3, 1);
    // 任意キーは VorbisComments に直接
    {
        use lofty::config::WriteOptions;
        use lofty::file::AudioFile;
        let mut f = File::open(&p).unwrap();
        let mut flac = lofty::flac::FlacFile::read_from(&mut f, Default::default()).unwrap();
        drop(f);
        flac.vorbis_comments_mut()
            .unwrap()
            .push("CUSTOM_KEY".to_owned(), "custom".to_owned());
        flac.save_to_path(&p, WriteOptions::default()).unwrap();
    }

    write(
        &p,
        "flac",
        &[
            change("TITLE", Some(&["新しい"])),
            change("ARTIST", Some(&["A", "B"])),
            change("ALBUMARTIST", None),
            change("NEW_KEY", Some(&["v"])),
        ],
    );
    assert_eq!(values(&p, "flac", "TITLE"), ["新しい"]);
    assert_eq!(values(&p, "flac", "ARTIST"), ["A", "B"]);
    assert!(values(&p, "flac", "ALBUMARTIST").is_empty());
    assert_eq!(values(&p, "flac", "NEW_KEY"), ["v"]);
    assert_eq!(values(&p, "flac", "CUSTOM_KEY"), ["custom"]);
    assert_eq!(values(&p, "flac", "ALBUM"), ["alb"]);
    assert_eq!(values(&p, "flac", "TRACKNUMBER"), ["3"]);
    // 再生できる（音声属性が読める）
    let af = read_audio_file(File::open(&p).unwrap(), Some("flac")).unwrap();
    assert_eq!(af.sample_rate, Some(44_100));
}

#[test]
fn opus_and_vorbis_write_vorbis_comments() {
    let dir = tempfile::tempdir().unwrap();
    for (name, ext) in [("a.opus", "opus"), ("a.ogg", "ogg")] {
        let p = require_ffmpeg!(common::make_audio(dir.path(), name, ext, 0));
        common::set_basic_tags(&p, "t", "art", "alb", "aa", 3, 1);
        write(
            &p,
            ext,
            &[
                change("TITLE", Some(&["新しい"])),
                change("ARTIST", Some(&["A", "B"])),
                change("ALBUMARTIST", None),
            ],
        );
        assert_eq!(values(&p, ext, "TITLE"), ["新しい"], "{ext}");
        assert_eq!(values(&p, ext, "ARTIST"), ["A", "B"], "{ext}");
        assert!(values(&p, ext, "ALBUMARTIST").is_empty(), "{ext}");
        assert_eq!(values(&p, ext, "ALBUM"), ["alb"], "{ext}");
    }
}

#[test]
fn mp4_and_mp3_map_vorbis_names_to_native_tags() {
    let dir = tempfile::tempdir().unwrap();
    for (name, ext) in [("a.m4a", "m4a"), ("a.mp3", "mp3")] {
        let p = require_ffmpeg!(common::make_audio(dir.path(), name, ext, 0));
        common::set_basic_tags(&p, "t", "art", "alb", "aa", 3, 1);
        write(
            &p,
            ext,
            &[
                change("TITLE", Some(&["新しい"])),
                change("ALBUMARTIST", None),
                change("MUSICBRAINZ_ALBUMID", Some(&["mbid-1"])),
            ],
        );
        assert_eq!(values(&p, ext, "TITLE"), ["新しい"], "{ext}");
        assert!(values(&p, ext, "ALBUMARTIST").is_empty(), "{ext}");
        assert_eq!(values(&p, ext, "MUSICBRAINZ_ALBUMID"), ["mbid-1"], "{ext}");
        assert_eq!(values(&p, ext, "ALBUM"), ["alb"], "{ext}");
        assert_eq!(values(&p, ext, "ARTIST"), ["art"], "{ext}");
    }
}

#[test]
fn keys_are_uppercased_and_values_nfc_normalized() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.flac", "flac", 0));
    // NFD の「が」(か + 濁点) → 読み戻しは NFC の「が」
    write(&p, "flac", &[change("title", Some(&["\u{304B}\u{3099}"]))]);
    assert_eq!(values(&p, "flac", "TITLE"), ["\u{304C}"]);
}

#[test]
fn mp3_with_id3v1_and_id3v2_updates_both_blocks() {
    use lofty::config::WriteOptions;
    use lofty::file::TaggedFileExt;
    use lofty::tag::{Accessor, TagExt, TagType};

    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.mp3", "mp3", 0));
    common::set_basic_tags(&p, "old", "art", "alb", "aa", 3, 1);
    // ID3v1 も付ける（同じ TITLE）
    {
        let mut v1 = lofty::tag::Tag::new(TagType::Id3v1);
        v1.set_title("old".to_owned());
        v1.set_artist("art".to_owned());
        v1.save_to_path(&p, WriteOptions::default()).unwrap();
        let tagged = lofty::read_from_path(&p).unwrap();
        assert_eq!(tagged.tags().len(), 2, "ID3v1 + ID3v2 の 2 ブロックが前提");
    }
    write(&p, "mp3", &[change("TITLE", Some(&["new"]))]);
    // 両ブロックが更新され、読み側（全ブロック集約）で旧値が残らない
    assert_eq!(values(&p, "mp3", "TITLE"), ["new"]);
    // 各ブロックを直接見ても両方 new（primary 優先の集約に隠れていない）
    let tagged = lofty::read_from_path(&p).unwrap();
    for tt in [TagType::Id3v2, TagType::Id3v1] {
        let tag = tagged.tag(tt).unwrap_or_else(|| panic!("{tt:?} が無い"));
        assert_eq!(tag.title().as_deref(), Some("new"), "{tt:?}");
    }
}

#[test]
fn mp4_keeps_multi_values_and_pictures() {
    use lofty::picture::{MimeType, Picture, PictureType};

    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.m4a", "m4a", 0));
    common::set_basic_tags(&p, "t", "art", "alb", "aa", 3, 1);
    common::retag(&p, |t| {
        t.push_picture(
            Picture::unchecked(vec![0x89, b'P', b'N', b'G', 1, 2, 3])
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Png)
                .build(),
        );
    });
    let before = read_audio_file(File::open(&p).unwrap(), Some("m4a")).unwrap();
    assert!(
        before.tags.first("PICTURE").is_some(),
        "画像が付いている前提"
    );

    write(
        &p,
        "m4a",
        &[
            change("ARTIST", Some(&["A", "B"])),
            change("TITLE", Some(&["x"])),
        ],
    );
    assert_eq!(values(&p, "m4a", "ARTIST"), ["A", "B"]);
    assert_eq!(values(&p, "m4a", "TITLE"), ["x"]);
    let after = read_audio_file(File::open(&p).unwrap(), Some("m4a")).unwrap();
    assert_eq!(after.tags.first("PICTURE"), before.tags.first("PICTURE"));
}

#[test]
fn flac_keeps_pictures_across_write() {
    use lofty::picture::{MimeType, Picture, PictureType};

    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.flac", "flac", 0));
    common::retag(&p, |t| {
        t.push_picture(
            Picture::unchecked(vec![0xff, 0xd8, 9, 9, 9])
                .pic_type(PictureType::CoverFront)
                .mime_type(MimeType::Jpeg)
                .build(),
        );
    });
    let before = read_audio_file(File::open(&p).unwrap(), Some("flac")).unwrap();
    assert!(before.tags.first("PICTURE").is_some());
    write(&p, "flac", &[change("TITLE", Some(&["x"]))]);
    let after = read_audio_file(File::open(&p).unwrap(), Some("flac")).unwrap();
    assert_eq!(after.tags.first("PICTURE"), before.tags.first("PICTURE"));
    assert_eq!(values(&p, "flac", "TITLE"), ["x"]);
}

fn ffprobe_tags(p: &Path) -> String {
    let out = std::process::Command::new("ffprobe")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-show_entries",
            "format_tags",
            "-of",
            "flat",
        ])
        .arg(p)
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// P4-11: 写像表に無いキーはフリーフォーム atom として書き、同じキーで読み戻せる（多値も）
#[test]
fn mp4_writes_arbitrary_keys_as_freeform_and_reads_them_back() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.m4a", "alac.m4a", 0));
    common::set_basic_tags(&p, "t", "art", "alb", "aa", 3, 1);
    write(
        &p,
        "m4a",
        &[
            change("SPINDLETEST", Some(&["one", "two"])),
            change("ITUNNORM", Some(&[" 00000000 00000000"])),
            change("CATALOGNUMBER", Some(&["CAT-1"])),
        ],
    );
    assert_eq!(values(&p, "m4a", "SPINDLETEST"), ["one", "two"]);
    assert_eq!(values(&p, "m4a", "ITUNNORM"), [" 00000000 00000000"]);
    assert_eq!(values(&p, "m4a", "CATALOGNUMBER"), ["CAT-1"]);
    assert_eq!(values(&p, "m4a", "ALBUM"), ["alb"]);
    let text = ffprobe_tags(&p);
    // フリーフォームの atom 名は内部キーのまま。iTunNORM だけ Apple の綴りに固定する
    assert!(text.contains("SPINDLETEST="), "{text}");
    assert!(text.contains("iTunNORM="), "{text}");
    assert!(!text.contains("ITUNNORM="), "{text}");

    // 削除はフリーフォームでも効く。外部が小文字で書いた同名 atom も一緒に消える
    write(&p, "m4a", &[change("SPINDLETEST", None)]);
    assert!(values(&p, "m4a", "SPINDLETEST").is_empty());
    assert_eq!(values(&p, "m4a", "ITUNNORM"), [" 00000000 00000000"]);
}

/// 大小文字違いの既存フリーフォームは置き換え時に二重化しない
#[test]
fn mp4_replaces_freeform_atoms_case_insensitively() {
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile as _;
    use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst};
    use std::io::Seek as _;

    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.m4a", "alac.m4a", 0));
    {
        let mut f = File::options().read(true).write(true).open(&p).unwrap();
        let mut mp4 = lofty::mp4::Mp4File::read_from(&mut f, ParseOptions::new()).unwrap();
        let mut ilst = mp4.ilst().cloned().unwrap_or_else(Ilst::new);
        ilst.insert(Atom::new(
            AtomIdent::Freeform {
                mean: "com.apple.iTunes".into(),
                name: "MyKey".into(),
            },
            AtomData::UTF8("old".into()),
        ));
        mp4.set_ilst(ilst);
        f.seek(std::io::SeekFrom::Start(0)).unwrap();
        mp4.save_to(&mut f, WriteOptions::default()).unwrap();
    }
    assert_eq!(values(&p, "m4a", "MYKEY"), ["old"]);
    write(&p, "m4a", &[change("MYKEY", Some(&["new"]))]);
    assert_eq!(values(&p, "m4a", "MYKEY"), ["new"]);
    let text = ffprobe_tags(&p);
    assert!(
        !text.contains("MyKey="),
        "旧綴りの atom が残っている: {text}"
    );
}

/// 標準キーは標準 atom のまま（フリーフォームに逃がさない）。trkn / disk の対は片側だけの
/// 変更でもう片側を壊さない
#[test]
fn mp4_keeps_standard_keys_in_standard_atoms_and_pairs_intact() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.m4a", "alac.m4a", 0));
    common::set_basic_tags(&p, "t", "art", "alb", "aa", 3, 1);
    write(
        &p,
        "m4a",
        &[
            change("TITLE", Some(&["新しい"])),
            change("GENRE", Some(&["J-Pop", "Anime"])),
            change("TRACKTOTAL", Some(&["12"])),
            change("DISCNUMBER", Some(&["2"])),
            change("COMPILATION", Some(&["1"])),
        ],
    );
    assert_eq!(values(&p, "m4a", "TITLE"), ["新しい"]);
    assert_eq!(values(&p, "m4a", "GENRE"), ["J-Pop", "Anime"]);
    assert_eq!(values(&p, "m4a", "TRACKNUMBER"), ["3"]);
    assert_eq!(values(&p, "m4a", "TRACKTOTAL"), ["12"]);
    assert_eq!(values(&p, "m4a", "DISCNUMBER"), ["2"]);
    assert_eq!(values(&p, "m4a", "COMPILATION"), ["1"]);
    let text = ffprobe_tags(&p);
    assert!(text.contains("format.tags.title=\"新しい\""), "{text}");
    assert!(text.contains("format.tags.track=\"3/12\""), "{text}");
    assert!(text.contains("format.tags.disc=\"2\""), "{text}");
    assert!(text.contains("format.tags.compilation=\"1\""), "{text}");
    assert!(
        !text.contains("TITLE="),
        "標準キーがフリーフォームに逃げている: {text}"
    );

    // TRACKNUMBER を消しても TRACKTOTAL は残る
    write(&p, "m4a", &[change("TRACKNUMBER", None)]);
    assert!(values(&p, "m4a", "TRACKNUMBER").is_empty());
    assert_eq!(values(&p, "m4a", "TRACKTOTAL"), ["12"]);
}

/// lofty で m4a の ilst にフリーフォーム atom を直接置く
fn put_freeform(path: &Path, mean: &str, name: &str, value: &str) {
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile as _;
    use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst};
    use std::io::Seek as _;

    let mut f = File::options().read(true).write(true).open(path).unwrap();
    let mut mp4 = lofty::mp4::Mp4File::read_from(&mut f, ParseOptions::new()).unwrap();
    let mut ilst = mp4.ilst().cloned().unwrap_or_else(Ilst::new);
    ilst.insert(Atom::new(
        AtomIdent::Freeform {
            mean: mean.to_owned().into(),
            name: name.to_owned().into(),
        },
        AtomData::UTF8(value.to_owned()),
    ));
    mp4.set_ilst(ilst);
    f.seek(std::io::SeekFrom::Start(0)).unwrap();
    mp4.save_to(&mut f, WriteOptions::default()).unwrap();
}

/// trkn の両側を同時に消すと atom ごと消える
#[test]
fn mp4_deleting_both_halves_removes_the_pair_atom() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.m4a", "alac.m4a", 0));
    common::set_basic_tags(&p, "t", "art", "alb", "aa", 3, 1);
    write(&p, "m4a", &[change("TRACKTOTAL", Some(&["12"]))]);
    assert!(ffprobe_tags(&p).contains("format.tags.track=\"3/12\""));
    write(
        &p,
        "m4a",
        &[change("TRACKNUMBER", None), change("TRACKTOTAL", None)],
    );
    assert!(values(&p, "m4a", "TRACKNUMBER").is_empty());
    assert!(values(&p, "m4a", "TRACKTOTAL").is_empty());
    let text = ffprobe_tags(&p);
    assert!(!text.contains("format.tags.track="), "{text}");
}

/// 標準 atom に写像されるフリーフォーム（CATALOGNUMBER）も、大小文字違いの既存 atom を置き換える
#[test]
fn mp4_replaces_mapped_freeform_case_insensitively() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.m4a", "alac.m4a", 0));
    put_freeform(&p, "com.apple.iTunes", "catalognumber", "old");
    assert_eq!(values(&p, "m4a", "CATALOGNUMBER"), ["old"]);
    write(&p, "m4a", &[change("CATALOGNUMBER", Some(&["CAT-2"]))]);
    assert_eq!(values(&p, "m4a", "CATALOGNUMBER"), ["CAT-2"]);
    let text = ffprobe_tags(&p);
    assert!(text.contains("CATALOGNUMBER=\"CAT-2\""), "{text}");
    assert!(
        !text.contains("catalognumber="),
        "旧綴りが残っている: {text}"
    );
}

/// `com.apple.iTunes` 以外の mean のフリーフォームは読まず、同名キーの書き込みでも消さない
#[test]
fn mp4_leaves_other_mean_freeform_atoms_alone() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.m4a", "alac.m4a", 0));
    put_freeform(&p, "org.example", "SPINDLETEST", "theirs");
    assert!(values(&p, "m4a", "SPINDLETEST").is_empty());
    write(&p, "m4a", &[change("SPINDLETEST", Some(&["ours"]))]);
    assert_eq!(values(&p, "m4a", "SPINDLETEST"), ["ours"]);
    write(&p, "m4a", &[change("SPINDLETEST", None)]);
    assert!(values(&p, "m4a", "SPINDLETEST").is_empty());
    // lofty で直接読み、他の mean の atom が残っていることを確かめる
    use lofty::config::ParseOptions;
    use lofty::file::AudioFile as _;
    let mp4 =
        lofty::mp4::Mp4File::read_from(&mut File::open(&p).unwrap(), ParseOptions::new()).unwrap();
    let ilst = mp4.ilst().unwrap();
    let other = ilst.get(&lofty::mp4::AtomIdent::Freeform {
        mean: "org.example".into(),
        name: "SPINDLETEST".into(),
    });
    assert!(other.is_some(), "他の mean の atom が消えた");
}

/// mp3 の ID3v2 に TXXX を直接置く（外部ツールが書いた独自フィールドの模擬）
fn put_txxx(path: &Path, description: &str, value: &str) {
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile as _;
    use std::io::Seek as _;

    let mut f = File::options().read(true).write(true).open(path).unwrap();
    let mut mp3 = lofty::mpeg::MpegFile::read_from(&mut f, ParseOptions::new()).unwrap();
    let mut id3 = mp3.id3v2().cloned().unwrap_or_default();
    let _ = id3.insert_user_text(description.to_owned(), value.to_owned());
    mp3.set_id3v2(id3);
    f.seek(std::io::SeekFrom::Start(0)).unwrap();
    mp3.save_to(&mut f, WriteOptions::default()).unwrap();
}

/// mp3 の TXXX を description ごとに集める（lofty で直接読む）
fn txxx(path: &Path) -> Vec<(String, String)> {
    use lofty::config::ParseOptions;
    use lofty::file::AudioFile as _;
    use lofty::id3::v2::{ExtendedTextFrame, Frame};

    let mp3 = lofty::mpeg::MpegFile::read_from(&mut File::open(path).unwrap(), ParseOptions::new())
        .unwrap();
    let Some(id3) = mp3.id3v2() else {
        return Vec::new();
    };
    id3.into_iter()
        .filter_map(|f| match f {
            Frame::UserText(ExtendedTextFrame {
                description,
                content,
                ..
            }) => Some((description.to_string(), content.to_string())),
            _ => None,
        })
        .collect()
}

/// D-110: 写像表に無いキーは ID3v2 の TXXX（description = キー）として書き、同じキーで読み戻せる
/// （多値も）。標準キーは標準フレームのまま
#[test]
fn mp3_writes_arbitrary_keys_as_txxx_and_reads_them_back() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.mp3", "mp3", 0));
    common::set_basic_tags(&p, "t", "art", "alb", "aa", 3, 1);
    write(
        &p,
        "mp3",
        &[
            change("SPINDLETEST", Some(&["one", "two"])),
            change("PLAYLIST_EXCLUDE", Some(&["1"])),
            change("TITLE", Some(&["new"])),
        ],
    );
    assert_eq!(values(&p, "mp3", "SPINDLETEST"), ["one", "two"]);
    assert_eq!(values(&p, "mp3", "PLAYLIST_EXCLUDE"), ["1"]);
    assert_eq!(values(&p, "mp3", "TITLE"), ["new"]);
    assert_eq!(values(&p, "mp3", "ALBUM"), ["alb"]);
    let frames = txxx(&p);
    assert!(
        frames.iter().any(|(d, _)| d == "PLAYLIST_EXCLUDE"),
        "{frames:?}"
    );
    assert!(
        !frames.iter().any(|(d, _)| d.eq_ignore_ascii_case("TITLE")),
        "標準キーが TXXX に逃げている: {frames:?}"
    );

    write(&p, "mp3", &[change("SPINDLETEST", None)]);
    assert!(values(&p, "mp3", "SPINDLETEST").is_empty());
    assert_eq!(values(&p, "mp3", "PLAYLIST_EXCLUDE"), ["1"]);
}

/// 外部が書いた TXXX は読み、別のキーの書き込みでは消さない。大小文字違いは置き換えで二重化しない
#[test]
fn mp3_keeps_foreign_txxx_and_replaces_case_insensitively() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.mp3", "mp3", 0));
    common::set_basic_tags(&p, "t", "art", "alb", "aa", 3, 1);
    put_txxx(&p, "MyKey", "old");
    put_txxx(&p, "Other Field", "keep");
    assert_eq!(values(&p, "mp3", "MYKEY"), ["old"]);
    assert_eq!(values(&p, "mp3", "OTHER FIELD"), ["keep"]);

    write(&p, "mp3", &[change("TITLE", Some(&["x"]))]);
    assert_eq!(
        values(&p, "mp3", "OTHER FIELD"),
        ["keep"],
        "無関係な TXXX が消えた"
    );

    write(&p, "mp3", &[change("MYKEY", Some(&["new"]))]);
    assert_eq!(values(&p, "mp3", "MYKEY"), ["new"]);
    let frames = txxx(&p);
    assert!(
        !frames.iter().any(|(d, _)| d == "MyKey"),
        "旧綴りの TXXX が残っている: {frames:?}"
    );
}

/// lofty が名前を知っている TXXX（REPLAYGAIN_*）は標準の綴りで書き、外部の小文字綴りを残さない
#[test]
fn mp3_mapped_txxx_replaces_lowercase_variant() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.mp3", "mp3", 0));
    common::set_basic_tags(&p, "t", "art", "alb", "aa", 3, 1);
    put_txxx(&p, "replaygain_track_gain", "-1.00 dB");
    assert_eq!(values(&p, "mp3", "REPLAYGAIN_TRACK_GAIN"), ["-1.00 dB"]);
    write(
        &p,
        "mp3",
        &[change("REPLAYGAIN_TRACK_GAIN", Some(&["-7.50 dB"]))],
    );
    assert_eq!(values(&p, "mp3", "REPLAYGAIN_TRACK_GAIN"), ["-7.50 dB"]);
    let frames = txxx(&p);
    let rg: Vec<_> = frames
        .iter()
        .filter(|(d, _)| d.eq_ignore_ascii_case("REPLAYGAIN_TRACK_GAIN"))
        .collect();
    assert_eq!(rg.len(), 1, "{frames:?}");
}

/// Vorbis 名の別名（TOTALTRACKS → TRACKTOTAL 等）は ID3v2 の標準フレームに写像すると別のキーで
/// 読み戻されるので、TXXX に書いて同じキーで読み戻す
#[test]
fn mp3_alias_keys_round_trip_under_their_own_name() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.mp3", "mp3", 0));
    common::set_basic_tags(&p, "t", "art", "alb", "aa", 3, 1);
    put_txxx(&p, "TOTALTRACKS", "9");
    assert_eq!(values(&p, "mp3", "TOTALTRACKS"), ["9"]);
    write(&p, "mp3", &[change("TOTALTRACKS", Some(&["12"]))]);
    assert_eq!(values(&p, "mp3", "TOTALTRACKS"), ["12"]);
    assert_eq!(values(&p, "mp3", "TRACKNUMBER"), ["3"], "TRCK を壊した");
}

/// description の大小文字は読み側と同じ（Unicode の大文字化）で比べる。ASCII 以外でも二重化しない
#[test]
fn mp3_replaces_non_ascii_txxx_case_insensitively() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.mp3", "mp3", 0));
    put_txxx(&p, "äkey", "old");
    assert_eq!(values(&p, "mp3", "ÄKEY"), ["old"]);
    write(&p, "mp3", &[change("ÄKEY", Some(&["new"]))]);
    assert_eq!(values(&p, "mp3", "ÄKEY"), ["new"]);
}

/// TRCK / TPOS は番号と総数が同居する。片側だけの変更で相方を保つ
#[test]
fn mp3_keeps_the_other_half_of_number_pairs() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.mp3", "mp3", 0));
    write(
        &p,
        "mp3",
        &[
            change("TRACKNUMBER", Some(&["3"])),
            change("TRACKTOTAL", Some(&["10"])),
            change("DISCNUMBER", Some(&["1"])),
            change("DISCTOTAL", Some(&["2"])),
        ],
    );
    write(&p, "mp3", &[change("TRACKNUMBER", Some(&["5"]))]);
    write(&p, "mp3", &[change("DISCTOTAL", Some(&["3"]))]);
    assert_eq!(values(&p, "mp3", "TRACKNUMBER"), ["5"]);
    assert_eq!(values(&p, "mp3", "TRACKTOTAL"), ["10"]);
    assert_eq!(values(&p, "mp3", "DISCNUMBER"), ["1"]);
    assert_eq!(values(&p, "mp3", "DISCTOTAL"), ["3"]);
}

/// APIC は画像を指定しない書き込みで保ち、指定すれば差し替える
#[test]
fn mp3_keeps_and_replaces_pictures() {
    use lofty::picture::{MimeType, Picture, PictureType};

    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.mp3", "mp3", 0));
    let pic = |b: u8| {
        Picture::unchecked(vec![0xff, 0xd8, b, b, b])
            .pic_type(PictureType::CoverFront)
            .mime_type(MimeType::Jpeg)
            .build()
    };
    common::retag(&p, |t| t.push_picture(pic(1)));
    let before = read_audio_file(File::open(&p).unwrap(), Some("mp3")).unwrap();
    assert!(before.tags.first("PICTURE").is_some());
    write(&p, "mp3", &[change("PLAYLIST_EXCLUDE", Some(&["1"]))]);
    let kept = read_audio_file(File::open(&p).unwrap(), Some("mp3")).unwrap();
    assert_eq!(kept.tags.first("PICTURE"), before.tags.first("PICTURE"));

    let mut f = File::options().read(true).write(true).open(&p).unwrap();
    write_tag_changes(&mut f, Some("mp3"), &[], Some(&[pic(2)])).unwrap();
    drop(f);
    let replaced = read_audio_file(File::open(&p).unwrap(), Some("mp3")).unwrap();
    assert_eq!(replaced.tags.values("PICTURE").count(), 1);
    assert_ne!(replaced.tags.first("PICTURE"), before.tags.first("PICTURE"));
}

/// ID3v2 + ID3v1 + APE が同居する mp3 で、APE の写像できない項目と ID3v2 の COMM を保つ
#[test]
fn mp3_keeps_unknown_ape_items_and_comments() {
    use lofty::ape::{ApeItem, ApeTag};
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile as _;
    use lofty::tag::{Accessor, ItemValue};
    use std::io::Seek as _;

    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.mp3", "mp3", 0));
    common::set_basic_tags(&p, "old", "art", "alb", "aa", 3, 1);
    {
        let mut f = File::options().read(true).write(true).open(&p).unwrap();
        let mut mp3 = lofty::mpeg::MpegFile::read_from(&mut f, ParseOptions::new()).unwrap();
        let mut id3 = mp3.id3v2().cloned().unwrap();
        id3.set_comment("hello".to_owned());
        mp3.set_id3v2(id3);
        let mut ape = ApeTag::new();
        ape.set_title("old".to_owned());
        ape.insert(
            ApeItem::new("MyApeOnly".to_owned(), ItemValue::Text("ape".to_owned())).unwrap(),
        );
        mp3.set_ape(ape);
        let mut v1 = lofty::id3::v1::Id3v1Tag::new();
        v1.set_title("old".to_owned());
        mp3.set_id3v1(v1);
        f.seek(std::io::SeekFrom::Start(0)).unwrap();
        mp3.save_to(&mut f, WriteOptions::default()).unwrap();
    }
    write(&p, "mp3", &[change("TITLE", Some(&["new"]))]);
    assert_eq!(values(&p, "mp3", "TITLE"), ["new"]);
    assert_eq!(values(&p, "mp3", "COMMENT"), ["hello"]);
    let mp3 = lofty::mpeg::MpegFile::read_from(&mut File::open(&p).unwrap(), ParseOptions::new())
        .unwrap();
    let ape = mp3.ape().expect("APE が消えた");
    assert!(ape.get("MyApeOnly").is_some(), "APE の未知項目が消えた");
    assert_eq!(ape.title().as_deref(), Some("new"));
    assert_eq!(mp3.id3v1().and_then(|t| t.title()).as_deref(), Some("new"));
}

/// ID3v2.3 の mp3 も読み書きでき、書いた後は ID3v2.4 になる（NUL 区切りの多値は v2.4 の形。D-110）
#[test]
fn mp3_v23_input_is_written_as_v24() {
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile as _;
    use lofty::id3::v2::Id3v2Version;
    use std::io::Seek as _;

    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.mp3", "mp3", 0));
    common::set_basic_tags(&p, "t", "art", "alb", "aa", 3, 1);
    {
        let mut f = File::options().read(true).write(true).open(&p).unwrap();
        let mut mp3 = lofty::mpeg::MpegFile::read_from(&mut f, ParseOptions::new()).unwrap();
        let mut id3 = mp3.id3v2().cloned().unwrap();
        let _ = id3.insert_user_text("OLDKEY".to_owned(), "v23".to_owned());
        mp3.set_id3v2(id3);
        f.seek(std::io::SeekFrom::Start(0)).unwrap();
        mp3.save_to(&mut f, WriteOptions::default().use_id3v23(true))
            .unwrap();
    }
    let version = |p: &Path| {
        lofty::mpeg::MpegFile::read_from(&mut File::open(p).unwrap(), ParseOptions::new())
            .unwrap()
            .id3v2()
            .unwrap()
            .original_version()
    };
    assert_eq!(version(&p), Id3v2Version::V3);
    assert_eq!(values(&p, "mp3", "OLDKEY"), ["v23"]);
    write(&p, "mp3", &[change("SPINDLETEST", Some(&["a", "b"]))]);
    assert_eq!(version(&p), Id3v2Version::V4);
    assert_eq!(values(&p, "mp3", "SPINDLETEST"), ["a", "b"]);
    assert_eq!(values(&p, "mp3", "OLDKEY"), ["v23"]);
    assert_eq!(values(&p, "mp3", "TITLE"), ["t"]);
}
