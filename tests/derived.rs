//! Derived の純粋な判定（docs/TASKS.md P1-10、D-51）。期待パス、必要な処理の判定、Opus に書く
//! タグ集合

use lofty::picture::{MimeType, Picture, PictureType};

use spindle::domain::derived::{
    aac_profiles, bake_gain_db, eligible, expected_rel_path, opus_profiles, opus_tags, plan,
    Current, Plan, Target, Variant, VariantSettings,
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
        rg_ready: true,
    }
}

fn current() -> Current {
    Current {
        rel_path: "opus/J-Pop/A/B/01 t.opus".into(),
        src_audio_version: 2,
        src_tag_version: 3,
        src_artwork_id: Some(7),
        src_rg_scanned_at: Some(100),
        audio_profile: "opus:256:v1".into(),
        tag_profile: "opus:v1".into(),
    }
}

/// opus 系統の設定（256k、on）
fn settings() -> VariantSettings {
    let (audio_profile, tag_profile) = opus_profiles(256);
    VariantSettings {
        variant: Variant::Opus,
        enabled: true,
        audio_profile,
        tag_profile,
        lossy_sources: false,
        multi_value_separator: " & ".into(),
    }
}

fn picture(mime: MimeType, data: &[u8]) -> Picture {
    Picture::unchecked(data.to_vec())
        .pic_type(PictureType::CoverFront)
        .mime_type(mime)
        .build()
}

#[test]
fn expected_path_is_under_the_variant_dir_with_its_extension() {
    let o = Variant::Opus;
    assert_eq!(expected_rel_path(o, "A/B/01 t.flac"), "opus/A/B/01 t.opus");
    assert_eq!(expected_rel_path(o, "A/B/01.t.wav"), "opus/A/B/01.t.opus");
    assert_eq!(expected_rel_path(o, "A/B/noext"), "opus/A/B/noext.opus");
    assert_eq!(expected_rel_path(o, "x.flac"), "opus/x.opus");
    // ディレクトリ名のドットは拡張子ではない。先頭ドットだけの名前も拡張子扱いしない
    assert_eq!(expected_rel_path(o, "A.b/x"), "opus/A.b/x.opus");
    assert_eq!(expected_rel_path(o, "A/.hidden"), "opus/A/.hidden.opus");
    assert_eq!(expected_rel_path(Variant::Aac, "A/x.flac"), "aac/A/x.m4a");
}

#[test]
fn variant_names_and_profiles() {
    assert_eq!(Variant::Opus.as_str(), "opus");
    assert_eq!(Variant::Aac.as_str(), "aac");
    assert_eq!(Variant::parse("opus"), Some(Variant::Opus));
    assert_eq!(Variant::parse("aac"), Some(Variant::Aac));
    assert_eq!(Variant::parse("ogg"), None);
    assert_eq!(Variant::ALL, [Variant::Opus, Variant::Aac]);
    assert_eq!(
        opus_profiles(256),
        ("opus:256:v1".to_owned(), "opus:v1".to_owned())
    );
    assert_eq!(opus_profiles(128).0, "opus:128:v1");
    assert_eq!(
        aac_profiles(256, " & "),
        (
            "aac:256:48k:bake1".to_owned(),
            "aac:sep= & :itunnorm0:v1".to_owned()
        )
    );
}

#[test]
fn eligibility_requires_lossless_present_and_stereo_or_mono() {
    let o = settings();
    assert!(eligible(&o, &target()));
    assert!(eligible(
        &o,
        &Target {
            channels: Some(1),
            ..target()
        }
    ));
    // チャンネル数不明はマルチチャンネルかもしれないので対象外
    assert!(!eligible(
        &o,
        &Target {
            channels: None,
            ..target()
        }
    ));
    assert!(!eligible(
        &o,
        &Target {
            channels: Some(6),
            ..target()
        }
    ));
    assert!(!eligible(
        &o,
        &Target {
            lossless: false,
            ..target()
        }
    ));
    assert!(!eligible(
        &o,
        &Target {
            missing: true,
            ..target()
        }
    ));
    // opus は RG を見ない（タグ上書きで追随できる）
    assert!(eligible(
        &o,
        &Target {
            rg_ready: false,
            rg_scanned_at: None,
            ..target()
        }
    ));
}

/// aac 系統の設定（256k、on）
fn aac_settings(lossy_sources: bool) -> VariantSettings {
    let (audio_profile, tag_profile) = aac_profiles(256, " & ");
    VariantSettings {
        variant: Variant::Aac,
        enabled: true,
        audio_profile,
        tag_profile,
        lossy_sources,
        multi_value_separator: " & ".into(),
    }
}

fn aac_current() -> Current {
    Current {
        rel_path: "aac/J-Pop/A/B/01 t.m4a".into(),
        src_audio_version: 2,
        src_tag_version: 3,
        src_artwork_id: Some(7),
        src_rg_scanned_at: Some(100),
        audio_profile: "aac:256:48k:bake1".into(),
        tag_profile: "aac:sep= & :itunnorm0:v1".into(),
    }
}

#[test]
fn aac_eligibility_covers_lossy_sources_and_requires_rg() {
    let s = aac_settings(true);
    assert!(eligible(&s, &target()));
    // 非可逆は lossy_sources のときだけ
    let lossy = Target {
        lossless: false,
        ..target()
    };
    assert!(eligible(&s, &lossy));
    assert!(!eligible(&aac_settings(false), &lossy));
    // RG 未解析（時刻が無い / 値が無い）は待つ
    assert!(!eligible(
        &s,
        &Target {
            rg_scanned_at: None,
            rg_ready: false,
            ..target()
        }
    ));
    assert!(!eligible(
        &s,
        &Target {
            rg_ready: false,
            ..target()
        }
    ));
    // opus と同じ除外
    assert!(!eligible(
        &s,
        &Target {
            missing: true,
            ..target()
        }
    ));
    assert!(!eligible(
        &s,
        &Target {
            channels: Some(6),
            ..target()
        }
    ));
    assert!(!eligible(
        &s,
        &Target {
            channels: None,
            ..target()
        }
    ));
}

#[test]
fn aac_plan_reencodes_on_rg_generation_and_retags_on_tag_profile() {
    let s = aac_settings(true);
    assert_eq!(plan(&s, &target(), Some(&aac_current())), Plan::UpToDate);
    // RG の解析世代が変わった → 音声に入っているので Encode（opus は Retag）
    let t = Target {
        rg_scanned_at: Some(101),
        ..target()
    };
    assert_eq!(plan(&s, &t, Some(&aac_current())), Plan::Encode);
    assert_eq!(plan(&settings(), &t, Some(&current())), Plan::Retag);
    // tag_profile（区切り）の差分は Retag
    let c = Current {
        tag_profile: "aac:sep= / :itunnorm0:v1".into(),
        ..aac_current()
    };
    assert_eq!(plan(&s, &target(), Some(&c)), Plan::Retag);
    // audio_profile（bitrate）の差分は Encode
    let c = Current {
        audio_profile: "aac:192:48k:bake1".into(),
        ..aac_current()
    };
    assert_eq!(plan(&s, &target(), Some(&c)), Plan::Encode);
    // 行が無ければ Encode、RG 未解析なら行があっても Skip（待つ）
    assert_eq!(plan(&s, &target(), None), Plan::Encode);
    let t = Target {
        rg_ready: false,
        ..target()
    };
    assert_eq!(plan(&s, &t, Some(&aac_current())), Plan::Skip);
    // lossy_sources を off にした後の非可逆の行は Skip（消さない）
    let t = Target {
        lossless: false,
        ..target()
    };
    assert_eq!(
        plan(&aac_settings(false), &t, Some(&aac_current())),
        Plan::Skip
    );
    // 凍結
    let frozen = VariantSettings {
        enabled: false,
        ..aac_settings(true)
    };
    assert_eq!(plan(&frozen, &target(), None), Plan::Skip);
}

#[test]
fn bake_gain_is_capped_by_true_peak_and_tolerates_non_finite_values() {
    let v = |g: f64, p: f64| Values {
        track_gain: g,
        track_peak: p,
        album_gain: None,
        album_peak: None,
    };
    let close = |a: f64, b: f64| (a - b).abs() < 1e-9;
    // peak 0.5 → 上限 +6.02 dB。gain が下ならそのまま
    assert!(close(bake_gain_db(&v(-6.0, 0.5)), -6.0));
    assert!(close(bake_gain_db(&v(3.0, 0.5)), 3.0));
    // 上限に掛かる
    assert!(close(bake_gain_db(&v(9.0, 0.5)), -20.0 * 0.5f64.log10()));
    // true peak > 1.0 は減衰側に倒れる
    assert!(close(bake_gain_db(&v(2.0, 2.0)), -20.0 * 2.0f64.log10()));
    assert!(bake_gain_db(&v(2.0, 2.0)) < 0.0);
    // peak が 0 以下 / 非有限なら上限なし
    assert!(close(bake_gain_db(&v(9.0, 0.0)), 9.0));
    assert!(close(bake_gain_db(&v(9.0, f64::NAN)), 9.0));
    assert!(close(bake_gain_db(&v(9.0, f64::INFINITY)), 9.0));
    // gain が非有限なら 0
    assert!(close(bake_gain_db(&v(f64::NAN, 0.5)), 0.0));
    assert!(close(bake_gain_db(&v(f64::NEG_INFINITY, 0.5)), 0.0));
}

#[test]
fn plan_covers_every_transition() {
    let t = target();
    assert_eq!(
        plan(
            &settings(),
            &Target {
                lossless: false,
                ..target()
            },
            Some(&current())
        ),
        Plan::Skip
    );
    assert_eq!(plan(&settings(), &t, None), Plan::Encode);
    assert_eq!(
        plan(
            &settings(),
            &t,
            Some(&Current {
                src_audio_version: 1,
                ..current()
            })
        ),
        Plan::Encode
    );
    assert_eq!(plan(&settings(), &t, Some(&current())), Plan::UpToDate);
    assert_eq!(
        plan(
            &settings(),
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
            &settings(),
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
            &settings(),
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
            &settings(),
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
            &settings(),
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
            &settings(),
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
            &settings(),
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
            &settings(),
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

/// 設定の世代（D-75）: audio_profile の差分は再エンコード、tag_profile の差分はタグ上書き。
/// enabled=false は凍結（行があっても何もしない）
#[test]
fn profiles_and_freeze_drive_the_plan() {
    let t = target();
    // 0018 が移した旧ルート直下の 128k の行。設定 256k → Encode（パスも opus/ 配下へ）
    let legacy = Current {
        rel_path: "J-Pop/A/B/01 t.opus".into(),
        audio_profile: "opus:128:v1".into(),
        ..current()
    };
    assert_eq!(plan(&settings(), &t, Some(&legacy)), Plan::Encode);
    // 設定を 128 のままにすれば profile 一致でパスの差分だけ → Move
    let (audio_profile, tag_profile) = opus_profiles(128);
    let s128 = VariantSettings {
        audio_profile,
        tag_profile,
        ..settings()
    };
    assert_eq!(plan(&s128, &t, Some(&legacy)), Plan::Move);
    // tag_profile だけ違えば Retag
    assert_eq!(
        plan(
            &settings(),
            &t,
            Some(&Current {
                tag_profile: "opus:v0".into(),
                ..current()
            })
        ),
        Plan::Retag
    );
    // 凍結: 行があっても無くても、古くても Skip
    let frozen = VariantSettings {
        enabled: false,
        ..settings()
    };
    assert_eq!(plan(&frozen, &t, None), Plan::Skip);
    assert_eq!(plan(&frozen, &t, Some(&legacy)), Plan::Skip);
    assert_eq!(
        plan(
            &frozen,
            &t,
            Some(&Current {
                src_tag_version: 1,
                ..current()
            })
        ),
        Plan::Skip
    );
    // aac 系統も凍結は Skip（対象の判定は aac_plan_* で見る）
    let aac = VariantSettings {
        enabled: false,
        ..aac_settings(true)
    };
    assert_eq!(plan(&aac, &t, None), Plan::Skip);
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
