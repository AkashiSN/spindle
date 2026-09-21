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
