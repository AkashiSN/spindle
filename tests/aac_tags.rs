//! aac 系統に書くタグ集合と MP4 への書き込み（SPEC §7.6「aac 系統」、D-75、docs/TASKS.md P4-8）。
//! 多値の結合、RG 系キーの除去、iTunNORM、標準 atom / フリーフォームの写像。ffmpeg / ffprobe が
//! 無ければ書き込みのテストは skip

#![cfg(target_os = "linux")]

mod common;

use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::Command;

use lofty::picture::{MimeType, Picture, PictureType};

use spindle::domain::derived::{aac_tags, ITUNNORM_KEY, ITUNNORM_ZERO_DB};
use spindle::domain::tags::{read_audio_file, read_transfer_tags, write_mp4_tags, TransferTags};

fn src_tags() -> TransferTags {
    TransferTags {
        items: vec![
            ("TITLE".into(), "曲".into()),
            ("ARTIST".into(), "A".into()),
            ("ALBUM".into(), "al".into()),
            ("ARTIST".into(), "B".into()),
            ("GENRE".into(), "Pop".into()),
            ("GENRE".into(), "Rock".into()),
            ("REPLAYGAIN_TRACK_GAIN".into(), "-6.00 dB".into()),
            ("REPLAYGAIN_TRACK_PEAK".into(), "0.5".into()),
            ("R128_TRACK_GAIN".into(), "-2816".into()),
            ("ITUNNORM".into(), " 00000B18 ...".into()),
            ("TRACKNUMBER".into(), "3".into()),
            ("TRACKTOTAL".into(), "12".into()),
            ("CATEGORY".into(), "Anime".into()),
            ("ARTIST".into(), "C".into()),
        ],
        pictures: vec![],
    }
}

fn values<'a>(t: &'a TransferTags, key: &str) -> Vec<&'a str> {
    t.items
        .iter()
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.as_str())
        .collect()
}

#[test]
fn aac_tags_join_multi_values_drop_rg_and_add_itunnorm() {
    let out = aac_tags(&src_tags(), " & ", None);
    // 同じキーは出現順に 1 値へ。1 値のキーはそのまま
    assert_eq!(values(&out, "ARTIST"), vec!["A & B & C"]);
    assert_eq!(values(&out, "GENRE"), vec!["Pop & Rock"]);
    assert_eq!(values(&out, "TITLE"), vec!["曲"]);
    assert_eq!(values(&out, "TRACKNUMBER"), vec!["3"]);
    // キーの順序は最初の出現順
    let keys: Vec<&str> = out.items.iter().map(|(k, _)| k.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "TITLE",
            "ARTIST",
            "ALBUM",
            "GENRE",
            "TRACKNUMBER",
            "TRACKTOTAL",
            "CATEGORY",
            ITUNNORM_KEY
        ]
    );
    // RG 系と元の ITUNNORM は無い。iTunNORM は 0 dB
    assert!(values(&out, "REPLAYGAIN_TRACK_GAIN").is_empty());
    assert!(values(&out, "REPLAYGAIN_TRACK_PEAK").is_empty());
    assert!(values(&out, "R128_TRACK_GAIN").is_empty());
    assert!(values(&out, "ITUNNORM").is_empty());
    assert_eq!(values(&out, ITUNNORM_KEY), vec![ITUNNORM_ZERO_DB]);
    assert_eq!(
        ITUNNORM_ZERO_DB,
        " 000003E8 000003E8 000009C4 000009C4 00000000 00000000 00000000 00000000 00000000 00000000"
    );
    assert!(out.pictures.is_empty());
    // 区切りは設定
    let out = aac_tags(&src_tags(), " / ", None);
    assert_eq!(values(&out, "ARTIST"), vec!["A / B / C"]);
}

#[test]
fn aac_tags_carry_the_cover() {
    let pic = Picture::unchecked(vec![0xFF, 0xD8, 0xFF, 0xD9])
        .pic_type(PictureType::CoverFront)
        .mime_type(MimeType::Jpeg)
        .build();
    let out = aac_tags(&src_tags(), " & ", Some(pic));
    assert_eq!(out.pictures.len(), 1);
    assert_eq!(out.pictures[0].mime_type(), Some(&MimeType::Jpeg));
}

/// ffmpeg で 64x64 の JPEG を作る
fn make_jpeg(ffmpeg: &Path, dir: &Path) -> PathBuf {
    let p = dir.join("cover.jpg");
    let st = Command::new(ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
            "color=c=red:s=64x64",
            "-frames:v",
            "1",
        ])
        .arg(&p)
        .status()
        .unwrap();
    assert!(st.success());
    p
}

fn ffprobe_tags(p: &Path) -> String {
    let out = Command::new("ffprobe")
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

#[test]
fn write_mp4_tags_maps_standard_atoms_and_freeform_and_covr() {
    let ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    let p = common::make_audio(dir.path(), "t.m4a", "m4a", 1).unwrap();
    let cover = Picture::unchecked(std::fs::read(make_jpeg(&ffmpeg, dir.path())).unwrap())
        .pic_type(PictureType::CoverFront)
        .mime_type(MimeType::Jpeg)
        .build();
    let out = aac_tags(&src_tags(), " & ", Some(cover));
    {
        let mut f = File::options().read(true).write(true).open(&p).unwrap();
        write_mp4_tags(&mut f, &out).unwrap();
    }
    // lofty で読み戻す（標準 atom は Vorbis 名に戻る）
    let af = read_audio_file(File::open(&p).unwrap(), Some("m4a")).unwrap();
    assert_eq!(af.codec.as_str(), "aac");
    assert_eq!(af.tags.first("TITLE"), Some("曲"));
    assert_eq!(af.tags.first("ARTIST"), Some("A & B & C"));
    assert_eq!(af.tags.first("ALBUM"), Some("al"));
    assert_eq!(af.tags.first("GENRE"), Some("Pop & Rock"));
    assert_eq!(af.tags.first("TRACKNUMBER"), Some("3"));
    assert_eq!(af.tags.first("TRACKTOTAL"), Some("12"));
    assert!(af.tags.first("REPLAYGAIN_TRACK_GAIN").is_none());
    assert_eq!(
        af.tags
            .first("PICTURE")
            .map(|s| s.starts_with("image/jpeg:")),
        Some(true)
    );
    // フリーフォーム（CATEGORY / iTunNORM）の atom 名と綴りは ffprobe で外部観測（P4-11 で読み側も取り込む）
    let text = ffprobe_tags(&p);
    assert!(
        text.contains(&format!("format.tags.iTunNORM=\"{ITUNNORM_ZERO_DB}\"")),
        "{text}"
    );
    assert!(text.contains("format.tags.CATEGORY=\"Anime\""), "{text}");
    assert!(
        !text.contains("ITUNNORM="),
        "大文字の atom は作らない: {text}"
    );
    // 2 回書いても増えず、画像なしで書けば消える（retag の置き換え）
    {
        let mut f = File::options().read(true).write(true).open(&p).unwrap();
        write_mp4_tags(&mut f, &aac_tags(&src_tags(), " / ", None)).unwrap();
    }
    let t = read_transfer_tags(File::open(&p).unwrap(), Some("m4a")).unwrap();
    assert_eq!(values(&t, "ARTIST"), vec!["A / B / C"]);
    assert!(t.pictures.is_empty());
    let text = ffprobe_tags(&p);
    assert_eq!(text.matches("iTunNORM=").count(), 1, "{text}");
}

/// 同じ ilst atom に写像される別名キー（LABEL / ORGANIZATION → ItemKey::Label）は後勝ちで 1 値
#[test]
fn write_mp4_tags_keeps_only_the_last_of_aliased_keys() {
    let _ffmpeg = require_ffmpeg!(common::ffmpeg());
    let dir = tempfile::tempdir().unwrap();
    let p = common::make_audio(dir.path(), "t.m4a", "m4a", 1).unwrap();
    let src = TransferTags {
        items: vec![
            ("TITLE".into(), "t".into()),
            ("LABEL".into(), "first".into()),
            ("ORGANIZATION".into(), "last".into()),
            ("TRACKTOTAL".into(), "5".into()),
            ("TOTALTRACKS".into(), "7".into()),
        ],
        pictures: vec![],
    };
    {
        let mut f = File::options().read(true).write(true).open(&p).unwrap();
        write_mp4_tags(&mut f, &aac_tags(&src, " & ", None)).unwrap();
    }
    let t = read_transfer_tags(File::open(&p).unwrap(), Some("m4a")).unwrap();
    assert_eq!(values(&t, "LABEL"), vec!["last"]);
    assert!(values(&t, "ORGANIZATION").is_empty());
    assert_eq!(values(&t, "TRACKTOTAL"), vec!["7"]);
}
