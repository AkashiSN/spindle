//! Derived の純粋な判定（docs/TASKS.md P1-10、D-51）。期待パス、必要な処理の判定、Opus に書く
//! タグ集合

use lofty::picture::{MimeType, Picture, PictureType};

use spindle::domain::derived::{
    eligible, expected_rel_path, opus_tags, plan, Current, Plan, Target,
};
use spindle::domain::replaygain::Values;
use spindle::domain::tags::TransferTags;

fn target() -> Target {
    Target {
        track_id: 1,
        lossless: true,
        missing: false,
        channels: Some(2),
        library_rel_path: "J-Pop/A/B/01 t.flac".into(),
        audio_version: 2,
        tag_version: 3,
        artwork_id: Some(7),
        rg_scanned_at: Some(100),
    }
}

fn current() -> Current {
    Current {
        rel_path: "J-Pop/A/B/01 t.opus".into(),
        src_audio_version: 2,
        src_tag_version: 3,
        src_artwork_id: Some(7),
        src_rg_scanned_at: Some(100),
    }
}

fn picture(mime: MimeType, data: &[u8]) -> Picture {
    Picture::unchecked(data.to_vec())
        .pic_type(PictureType::CoverFront)
        .mime_type(mime)
        .build()
}

#[test]
fn expected_path_replaces_extension_with_opus() {
    assert_eq!(expected_rel_path("A/B/01 t.flac"), "A/B/01 t.opus");
    assert_eq!(expected_rel_path("A/B/01.t.wav"), "A/B/01.t.opus");
    assert_eq!(expected_rel_path("A/B/noext"), "A/B/noext.opus");
    assert_eq!(expected_rel_path("x.flac"), "x.opus");
    // ディレクトリ名のドットは拡張子ではない。先頭ドットだけの名前も拡張子扱いしない
    assert_eq!(expected_rel_path("A.b/x"), "A.b/x.opus");
    assert_eq!(expected_rel_path("A/.hidden"), "A/.hidden.opus");
}

#[test]
fn eligibility_requires_lossless_present_and_stereo_or_mono() {
    assert!(eligible(&target()));
    assert!(eligible(&Target {
        channels: Some(1),
        ..target()
    }));
    // チャンネル数不明はマルチチャンネルかもしれないので対象外
    assert!(!eligible(&Target {
        channels: None,
        ..target()
    }));
    assert!(!eligible(&Target {
        channels: Some(6),
        ..target()
    }));
    assert!(!eligible(&Target {
        lossless: false,
        ..target()
    }));
    assert!(!eligible(&Target {
        missing: true,
        ..target()
    }));
}

#[test]
fn plan_covers_every_transition() {
    let t = target();
    assert_eq!(
        plan(
            &Target {
                lossless: false,
                ..target()
            },
            Some(&current())
        ),
        Plan::Skip
    );
    assert_eq!(plan(&t, None), Plan::Encode);
    assert_eq!(
        plan(
            &t,
            Some(&Current {
                src_audio_version: 1,
                ..current()
            })
        ),
        Plan::Encode
    );
    assert_eq!(plan(&t, Some(&current())), Plan::UpToDate);
    assert_eq!(
        plan(
            &t,
            Some(&Current {
                src_tag_version: 2,
                ..current()
            })
        ),
        Plan::Retag
    );
    assert_eq!(
        plan(
            &t,
            Some(&Current {
                src_artwork_id: None,
                ..current()
            })
        ),
        Plan::Retag
    );
    assert_eq!(
        plan(
            &Target {
                artwork_id: None,
                ..target()
            },
            Some(&current())
        ),
        Plan::Retag
    );
    // RG の解析世代（未解析 → 解析済み、再解析）もタグの上書き
    assert_eq!(
        plan(
            &t,
            Some(&Current {
                src_rg_scanned_at: None,
                ..current()
            })
        ),
        Plan::Retag
    );
    assert_eq!(
        plan(
            &t,
            Some(&Current {
                src_rg_scanned_at: Some(99),
                ..current()
            })
        ),
        Plan::Retag
    );
    assert_eq!(
        plan(
            &t,
            Some(&Current {
                rel_path: "old/x.opus".into(),
                ..current()
            })
        ),
        Plan::Move
    );
    assert_eq!(
        plan(
            &t,
            Some(&Current {
                rel_path: "old/x.opus".into(),
                src_tag_version: 1,
                ..current()
            })
        ),
        Plan::MoveAndRetag
    );
    // 音声版が古ければパスも含めて作り直す（Encode が全部面倒を見る）
    assert_eq!(
        plan(
            &t,
            Some(&Current {
                rel_path: "old/x.opus".into(),
                src_audio_version: 1,
                ..current()
            })
        ),
        Plan::Encode
    );
}

#[test]
fn opus_tags_transfer_items_replace_rg_and_pictures() {
    let src = TransferTags {
        items: vec![
            ("TITLE".into(), "t".into()),
            ("REPLAYGAIN_TRACK_GAIN".into(), "-1.00 dB".into()),
            ("REPLAYGAIN_ALBUM_PEAK".into(), "0.9".into()),
            ("R128_TRACK_GAIN".into(), "999".into()),
        ],
        pictures: vec![picture(MimeType::Jpeg, &[0xFF, 0xD8, 0xFF, 0])],
    };
    let rg = Values {
        track_gain: -6.0,
        track_peak: 0.5,
        album_gain: Some(-4.0),
        album_peak: Some(0.7),
    };
    let cover = picture(MimeType::Unknown("image/webp".into()), b"RIFF....WEBP");
    let out = opus_tags(&src, Some(&rg), -18.0, Some(cover));
    let keys: Vec<&str> = out.items.iter().map(|(k, _)| k.as_str()).collect();
    assert!(keys.contains(&"TITLE"));
    assert!(!keys.iter().any(|k| k.starts_with("REPLAYGAIN_")));
    // -18 基準の -6 dB → -23 基準では -11 dB → Q7.8 で -2816
    assert!(out
        .items
        .contains(&("R128_TRACK_GAIN".into(), "-2816".into())));
    assert!(out
        .items
        .contains(&("R128_ALBUM_GAIN".into(), "-2304".into())));
    assert_eq!(out.pictures.len(), 1);
    assert_eq!(
        out.pictures[0].mime_type(),
        Some(&MimeType::Unknown("image/webp".into()))
    );

    // album 無しなら R128_ALBUM_GAIN は書かない
    let out = opus_tags(
        &src,
        Some(&Values {
            album_gain: None,
            album_peak: None,
            ..rg
        }),
        -18.0,
        None,
    );
    assert!(out.items.iter().any(|(k, _)| k == "R128_TRACK_GAIN"));
    assert!(!out.items.iter().any(|(k, _)| k == "R128_ALBUM_GAIN"));

    // RG 未解析なら RG 系のキーは一切書かない。画像も無ければ空
    let out = opus_tags(&src, None, -18.0, None);
    assert!(!out
        .items
        .iter()
        .any(|(k, _)| k.starts_with("REPLAYGAIN_") || k.starts_with("R128_")));
    assert!(out.pictures.is_empty());
}
