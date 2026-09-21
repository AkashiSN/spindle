//! lofty ラッパによるタグ・音声属性の読み取り（SPEC §6、docs/TASKS.md P0-6）。
//! 形式差を Vorbis Comment 名に揃え、任意キー・多値を保つ。

mod common;

use std::fs::File;

use lofty::tag::{Accessor, ItemKey};

use spindle::domain::tags::{read_audio_file, Codec};

fn tags_of(path: &std::path::Path, ext: &str) -> spindle::domain::tags::AudioFile {
    read_audio_file(File::open(path).unwrap(), Some(ext)).unwrap()
}

#[test]
fn flac_keeps_arbitrary_keys_and_multi_values() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.flac", "flac", 0));
    common::retag(&p, |tag| {
        tag.set_title("曲名".to_owned());
        tag.push(lofty::tag::TagItem::new(
            ItemKey::TrackArtist,
            lofty::tag::ItemValue::Text("A".to_owned()),
        ));
        tag.push(lofty::tag::TagItem::new(
            ItemKey::TrackArtist,
            lofty::tag::ItemValue::Text("B".to_owned()),
        ));
        tag.insert_text(ItemKey::MusicBrainzReleaseId, "mbid-1".to_owned());
    });
    // 任意キーは lofty の generic Tag では落ちるので VorbisComments を直接書く
    {
        use lofty::config::WriteOptions;
        use lofty::file::AudioFile;
        let mut f = File::open(&p).unwrap();
        let mut flac = lofty::flac::FlacFile::read_from(&mut f, Default::default()).unwrap();
        drop(f);
        let vc = flac.vorbis_comments_mut().unwrap();
        vc.push("CUSTOM_KEY".to_owned(), "custom".to_owned());
        vc.push("lowercase".to_owned(), "x".to_owned());
        flac.save_to_path(&p, WriteOptions::default()).unwrap();
    }

    let af = tags_of(&p, "flac");
    assert_eq!(af.codec, Codec::Flac);
    assert!(af.lossless);
    assert_eq!(af.sample_rate, Some(44_100));
    assert_eq!(af.bit_depth, Some(16));
    assert_eq!(af.channels, Some(2));
    assert!(
        (900..=1100).contains(&af.duration_ms.unwrap()),
        "{:?}",
        af.duration_ms
    );

    let items = af.tags.items();
    let get = |k: &str| -> Vec<&str> {
        items
            .iter()
            .filter(|(key, _)| key == k)
            .map(|(_, v)| v.as_str())
            .collect()
    };
    assert_eq!(get("TITLE"), ["曲名"]);
    assert_eq!(get("ARTIST"), ["A", "B"], "多値の順序を保つ");
    assert_eq!(get("MUSICBRAINZ_ALBUMID"), ["mbid-1"]);
    assert_eq!(get("CUSTOM_KEY"), ["custom"], "任意キーを落とさない");
    assert_eq!(get("LOWERCASE"), ["x"], "キーは大文字に正規化");
}

#[test]
fn opus_reads_vorbis_comments_and_is_lossy() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.opus", "opus", 0));
    common::set_basic_tags(&p, "T", "Ar", "Al", "AA", 3, 1);
    let af = tags_of(&p, "opus");
    assert_eq!(af.codec, Codec::Opus);
    assert!(!af.lossless);
    assert_eq!(af.sample_rate, Some(48_000));
    assert!(af.bitrate.is_some());
    let items = af.tags.items();
    assert!(items.contains(&("TITLE".to_owned(), "T".to_owned())));
    assert!(items.contains(&("ALBUMARTIST".to_owned(), "AA".to_owned())));
    assert!(items.contains(&("TRACKNUMBER".to_owned(), "3".to_owned())));
    assert!(items.contains(&("DISCNUMBER".to_owned(), "1".to_owned())));
}

#[test]
fn mp3_id3v2_frames_are_mapped_to_vorbis_names() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.mp3", "mp3", 0));
    common::set_basic_tags(&p, "T", "Ar", "Al", "AA", 3, 1);
    let af = tags_of(&p, "mp3");
    assert_eq!(af.codec, Codec::Mp3);
    assert!(!af.lossless);
    let items = af.tags.items();
    assert!(items.contains(&("TITLE".to_owned(), "T".to_owned())));
    assert!(
        items.contains(&("ALBUMARTIST".to_owned(), "AA".to_owned())),
        "{items:?}"
    );
    assert!(
        items.contains(&("TRACKNUMBER".to_owned(), "3".to_owned())),
        "{items:?}"
    );
}

#[test]
fn m4a_distinguishes_aac_from_alac() {
    let dir = tempfile::tempdir().unwrap();
    let aac = require_ffmpeg!(common::make_audio(dir.path(), "a.m4a", "m4a", 0));
    let alac = common::make_audio(dir.path(), "b.m4a", "alac.m4a", 0).unwrap();
    common::set_basic_tags(&aac, "T", "Ar", "Al", "AA", 3, 1);
    let a = tags_of(&aac, "m4a");
    assert_eq!(a.codec, Codec::Aac);
    assert!(!a.lossless);
    assert!(a
        .tags
        .items()
        .contains(&("TITLE".to_owned(), "T".to_owned())));
    let b = tags_of(&alac, "m4a");
    assert_eq!(b.codec, Codec::Alac);
    assert!(b.lossless);
    assert_eq!(b.bit_depth, Some(16));
}

#[test]
fn wav_and_ogg_vorbis_are_recognized() {
    let dir = tempfile::tempdir().unwrap();
    let wav = dir.path().join("a.wav");
    common::write_wav(&wav, &common::pcm_samples(0), 16);
    let w = tags_of(&wav, "wav");
    assert_eq!(w.codec, Codec::Wav);
    assert!(w.lossless);
    assert_eq!(w.bit_depth, Some(16));

    let ogg = require_ffmpeg!(common::make_audio(dir.path(), "a.ogg", "ogg", 0));
    let o = tags_of(&ogg, "ogg");
    assert_eq!(o.codec, Codec::Ogg);
    assert!(!o.lossless);
}

#[test]
fn embedded_picture_changes_tag_hash() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.flac", "flac", 0));
    common::set_basic_tags(&p, "T", "Ar", "Al", "AA", 1, 1);
    let before = spindle::domain::tags::tag_hash(&tags_of(&p, "flac").tags);
    common::retag(&p, |tag| {
        let pic = lofty::picture::Picture::unchecked(b"\xff\xd8\xff\xe0fakejpeg".to_vec())
            .pic_type(lofty::picture::PictureType::CoverFront)
            .mime_type(lofty::picture::MimeType::Jpeg)
            .build();
        tag.push_picture(pic);
    });
    let after = spindle::domain::tags::tag_hash(&tags_of(&p, "flac").tags);
    assert_ne!(before, after);
}

#[test]
fn non_audio_file_is_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let p = dir.path().join("x.flac");
    std::fs::write(&p, b"not audio at all").unwrap();
    assert!(read_audio_file(File::open(&p).unwrap(), Some("flac")).is_err());
}

#[test]
fn codec_from_extension_matches_scanner_whitelist() {
    assert_eq!(Codec::from_extension("FLAC"), Some(Codec::Flac));
    assert_eq!(Codec::from_extension("m4a"), Some(Codec::Aac)); // 暫定。中身で ALAC に置き換わる
    assert_eq!(Codec::from_extension("txt"), None);
    assert_eq!(Codec::Opus.as_str(), "opus");
}

#[test]
fn mp3_with_two_tag_blocks_prefers_primary_and_fills_missing_keys_from_secondary() {
    use lofty::config::WriteOptions;
    use lofty::tag::{TagExt, TagType};

    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.mp3", "mp3", 0));
    common::retag(&p, |t| t.set_title("v2".to_owned()));
    let mut v1 = lofty::tag::Tag::new(TagType::Id3v1);
    v1.set_title("v1".to_owned());
    v1.set_artist("only-in-v1".to_owned());
    v1.save_to_path(&p, WriteOptions::default()).unwrap();

    let af = tags_of(&p, "mp3");
    let get = |k: &str| -> Vec<&str> { af.tags.values(k).collect() };
    // 同じキーは primary（ID3v2）だけ。副ブロックの同値・異値は多値にしない
    assert_eq!(get("TITLE"), ["v2"]);
    // primary に無いキーは副ブロックから補う
    assert_eq!(get("ARTIST"), ["only-in-v1"]);
}

/// `TagReadError::io_kind` は lofty の Parse に包まれた I/O エラーも source を辿って見つける
/// （Inbox の埋め込み画像が「内容が変わった」と「読めない」を分けるのに使う。P4-4）
#[test]
fn io_kind_finds_io_errors_wrapped_by_lofty() {
    use spindle::domain::tags::TagReadError;
    use std::io::{Error, ErrorKind};
    let wrapped = TagReadError::Parse(lofty::error::FileParseError::from(Error::new(
        ErrorKind::PermissionDenied,
        "denied",
    )));
    assert_eq!(wrapped.io_kind(), Some(ErrorKind::PermissionDenied));
    let direct = TagReadError::Io(Error::new(ErrorKind::UnexpectedEof, "eof"));
    assert_eq!(direct.io_kind(), Some(ErrorKind::UnexpectedEof));
    assert_eq!(TagReadError::Unrecognized.io_kind(), None);
}

/// lofty で m4a の ilst にフリーフォーム atom を直接置く（外部ツールが書いた独自キーの模擬）
fn put_freeform(path: &std::path::Path, entries: &[(&str, &[&str])]) {
    use lofty::config::{ParseOptions, WriteOptions};
    use lofty::file::AudioFile as _;
    use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst};

    let mut f = File::options().read(true).write(true).open(path).unwrap();
    let mut mp4 = lofty::mp4::Mp4File::read_from(&mut f, ParseOptions::new()).unwrap();
    let mut ilst = mp4.ilst().cloned().unwrap_or_else(Ilst::new);
    for (name, values) in entries {
        let data = values
            .iter()
            .map(|v| AtomData::UTF8((*v).to_owned()))
            .collect();
        let atom = Atom::from_collection(
            AtomIdent::Freeform {
                mean: "com.apple.iTunes".into(),
                name: (*name).to_owned().into(),
            },
            data,
        )
        .unwrap();
        ilst.insert(atom);
    }
    mp4.set_ilst(ilst);
    use std::io::Seek as _;
    f.seek(std::io::SeekFrom::Start(0)).unwrap();
    mp4.save_to(&mut f, WriteOptions::default()).unwrap();
}

#[test]
fn m4a_reads_freeform_atoms_as_uppercased_keys_with_multi_values() {
    let dir = tempfile::tempdir().unwrap();
    let p = require_ffmpeg!(common::make_audio(dir.path(), "a.m4a", "alac.m4a", 0));
    common::set_basic_tags(&p, "T", "Ar", "Al", "AA", 3, 1);
    put_freeform(
        &p,
        &[
            ("SPINDLETEST", &["x", "y"]),
            ("MyKey", &["v"]),
            ("iTunNORM", &[" 00000000 00000000"]),
            // 標準 atom（©alb）と同名のフリーフォームは標準 atom が勝つ
            ("ALBUM", &["shadow"]),
        ],
    );
    let af = tags_of(&p, "m4a");
    let vals = |k: &str| af.tags.values(k).collect::<Vec<_>>();
    assert_eq!(vals("SPINDLETEST"), ["x", "y"]);
    assert_eq!(vals("MYKEY"), ["v"]);
    assert_eq!(vals("ITUNNORM"), [" 00000000 00000000"]);
    assert_eq!(vals("ALBUM"), ["Al"]);
    // 標準 atom の写像は従来どおり
    assert_eq!(vals("TITLE"), ["T"]);
    assert_eq!(vals("TRACKNUMBER"), ["3"]);
}
