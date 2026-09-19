//! TOC と各種 DiscID（SPEC §7.2「ID 算出」、§7.3「TOC 再構成」）。
//! 受け入れ: docs/TASKS.md P2-2（ID 算出部分）
//!
//! 参照値は実在のディスクから取った:
//! - MusicBrainz「Disc ID Calculation」の 6 トラック例と CD-Extra（8 トラック、末尾がデータ）例。
//!   DiscID はドキュメント記載値、CD-Extra 例は MB の `ws/2/discid` で実在を確認
//! - Nirvana「Nevermind」（MB の `ws/2/release` から取った TOC）。AccurateRip ID は
//!   実サーバ（`dBAR-012-0013f127-00b61059-a109fe0c.bin`）のヘッダで確認
//! - Linkin Park「Hybrid Theory」JP Enhanced CD（CTDB の応答から取った TOC。データトラックあり）。
//!   AccurateRip ID は CUETools 式と音声セッション式の両方が実サーバにあることを確認

use spindle::cd::toc::{Toc, TocError, TocTrack};

fn audio(number: u8, start_lba: u32) -> TocTrack {
    TocTrack {
        number,
        start_lba,
        is_audio: true,
    }
}

fn data(number: u8, start_lba: u32) -> TocTrack {
    TocTrack {
        number,
        start_lba,
        is_audio: false,
    }
}

fn audio_toc(starts: &[u32], leadout: u32) -> Toc {
    let tracks = starts
        .iter()
        .enumerate()
        .map(|(i, &s)| audio(i as u8 + 1, s))
        .collect();
    Toc::new(tracks, leadout).expect("正しい TOC")
}

/// MusicBrainz ドキュメントの 6 トラック例（LBA）
fn mb_doc_disc() -> Toc {
    audio_toc(&[0, 15213, 32164, 46442, 63264, 80339], 95312)
}

/// Nirvana「Nevermind」（MB の offsets から 150 を引いた LBA、sectors 192030）
fn nevermind() -> Toc {
    audio_toc(
        &[
            0, 22593, 41700, 58133, 71920, 91198, 104468, 115188, 131988, 143758, 159678, 174415,
        ],
        191880,
    )
}

/// MusicBrainz ドキュメントの CD-Extra 例。8 トラック目がデータ
fn mb_doc_cd_extra() -> Toc {
    let mut tracks: Vec<TocTrack> = [0, 13959, 33436, 52927, 65631, 77742, 99024]
        .iter()
        .enumerate()
        .map(|(i, &s)| audio(i as u8 + 1, s))
        .collect();
    tracks.push(data(8, 125824));
    Toc::new(tracks, 188333).expect("正しい TOC")
}

/// Linkin Park「Hybrid Theory」JP Enhanced CD（CTDB: `0:13915:…:190165:-218477:241060`）
fn hybrid_theory() -> Toc {
    let starts = [
        0, 13915, 25592, 40835, 55855, 71530, 85325, 99560, 115782, 129627, 144212, 156000, 170495,
        190165,
    ];
    let mut tracks: Vec<TocTrack> = starts
        .iter()
        .enumerate()
        .map(|(i, &s)| audio(i as u8 + 1, s))
        .collect();
    tracks.push(data(15, 218477));
    Toc::new(tracks, 241060).expect("正しい TOC")
}

// ---------------------------------------------------------------- MusicBrainz DiscID

#[test]
fn musicbrainz_disc_id_matches_the_documented_example() {
    assert_eq!(
        mb_doc_disc().musicbrainz_disc_id(),
        "49HHV7Eb8UKF3aQiNmu1GR8vKTY-"
    );
}

#[test]
fn musicbrainz_disc_id_matches_a_real_release() {
    assert_eq!(
        nevermind().musicbrainz_disc_id(),
        "y6Br7t4P.bldLe_6Im2d9Z42IU4-"
    );
}

/// 末尾のデータトラックは数えず、リードアウトはデータトラック開始 − 11400
#[test]
fn musicbrainz_disc_id_drops_a_trailing_data_track_with_the_session_gap() {
    assert_eq!(
        mb_doc_cd_extra().musicbrainz_disc_id(),
        "BPnh1KU.hea1C.KMYWLGZkHJr0w-"
    );
    assert_eq!(
        hybrid_theory().musicbrainz_disc_id(),
        "2.tP0oankaC3cIx_zujEkKvXTv0-"
    );
}

/// `ws/2/discid/-?toc=` に渡す形（先頭・末尾トラック番号、リードアウト、各オフセット。+150 済み）
#[test]
fn musicbrainz_toc_string_lists_offsets_with_the_pregap() {
    assert_eq!(
        mb_doc_disc().musicbrainz_toc(),
        "1 6 95462 150 15363 32314 46592 63414 80489"
    );
    assert_eq!(
        mb_doc_cd_extra().musicbrainz_toc(),
        "1 7 114574 150 14109 33586 53077 65781 77892 99174"
    );
}

// ---------------------------------------------------------------- AccurateRip / FreeDB

#[test]
fn accuraterip_id_matches_the_server_for_a_real_release() {
    let id = nevermind().accuraterip_id();
    assert_eq!(id.to_string(), "0013f127-00b61059-a109fe0c");
    assert_eq!(id.audio_tracks, 12);
    assert_eq!(nevermind().freedb_id(), 0xa109fe0c);
}

#[test]
fn accuraterip_id_of_the_documented_example() {
    assert_eq!(
        mb_doc_disc().accuraterip_id().to_string(),
        "000513be-001b2231-3404f606"
    );
}

/// CUETools / dBpoweramp 式: id1 / id2 は音声トラックだけを足すが、リードアウトは実際の値。
/// FreeDB はデータトラックも数える
#[test]
fn accuraterip_id_of_an_enhanced_cd_uses_the_real_leadout_and_counts_the_data_track_in_freedb() {
    let id = hybrid_theory().accuraterip_id();
    assert_eq!(id.to_string(), "00177f71-00fe3bc2-d40c8e0f");
    assert_eq!(id.audio_tracks, 14);
}

/// 音声セッションだけの TOC にすると、データトラックを落とす流儀（libdiscid 系）の ID になる。
/// どちらも実サーバにエントリがある
#[test]
fn accuraterip_id_of_the_audio_session_is_the_alternative_key() {
    let session = hybrid_theory().audio_session();
    assert_eq!(session.tracks().len(), 14);
    assert_eq!(session.leadout_lba(), 218477 - 11400);
    assert_eq!(
        session.accuraterip_id().to_string(),
        "0016fab2-00f67491-c30ac90e"
    );
}

/// データトラックが無ければ音声セッションは元と同じ
#[test]
fn audio_session_of_a_plain_audio_cd_is_unchanged() {
    assert_eq!(nevermind().audio_session(), nevermind());
}

// ---------------------------------------------------------------- CTDB

/// CTDB の照会に使う TOC 文字列（データトラックは `-` 前置、末尾はリードアウト）
#[test]
fn ctdb_toc_string_marks_data_tracks() {
    assert_eq!(
        hybrid_theory().ctdb_toc(),
        "0:13915:25592:40835:55855:71530:85325:99560:115782:129627:144212:156000:170495:190165:-218477:241060"
    );
    assert_eq!(
        mb_doc_disc().ctdb_toc(),
        "0:15213:32164:46442:63264:80339:95312"
    );
}

/// TOCID は音声トラックの相対オフセットと音声部分の長さの SHA-1（CUETools `CDImageLayout.TOCID`）。
/// 参照値は scripts/gen_cd_crc_fixture.py と同じ流儀で独立に計算した
#[test]
fn ctdb_toc_id_hashes_relative_audio_offsets() {
    assert_eq!(
        hybrid_theory().ctdb_toc_id(),
        "th3rFI2I9a0fOix17ODEFIO0yU0-"
    );
}

// ---------------------------------------------------------------- レイアウトと再構成

/// CRC 用のレイアウトは音声トラックだけ。末尾がデータなら最後の音声トラックは −11400 で切る
#[test]
fn track_layout_covers_audio_tracks_only() {
    let lay = hybrid_theory().track_layout().expect("レイアウト");
    assert_eq!(lay.track_count(), 14);
    assert_eq!(lay.lengths()[0], 13915 * 588);
    assert_eq!(lay.lengths()[13], (218477 - 11400 - 190165) * 588);
    let lay = nevermind().track_layout().expect("レイアウト");
    assert_eq!(lay.lengths()[11], (191880 - 174415) * 588);
}

/// §7.3: サンプル数から TOC を再構成。先頭は LBA 0、以降は前トラックのセクタ数の累積
#[test]
fn toc_is_reconstructed_from_audio_sample_counts() {
    let sectors = [15213u64, 16951, 14278, 16822, 17075, 14973];
    let toc = Toc::from_audio_sample_counts(sectors.iter().map(|s| s * 588)).expect("再構成");
    assert_eq!(toc, mb_doc_disc());
    assert_eq!(toc.musicbrainz_disc_id(), "49HHV7Eb8UKF3aQiNmu1GR8vKTY-");
}

/// 588 の倍数でないサンプル数は CD 由来でないので拒否する
#[test]
fn reconstruction_rejects_sample_counts_that_are_not_sector_aligned() {
    let r = Toc::from_audio_sample_counts([15213 * 588, 16951 * 588 + 1]);
    assert_eq!(
        r,
        Err(TocError::NotSectorAligned {
            index: 1,
            samples: 16951 * 588 + 1
        })
    );
}

// ---------------------------------------------------------------- 検証

#[test]
fn toc_rejects_malformed_track_lists() {
    assert_eq!(Toc::new(vec![], 1000), Err(TocError::NoTracks));
    assert_eq!(
        Toc::new(vec![audio(1, 0), audio(3, 100)], 1000),
        Err(TocError::NonConsecutiveNumbers { index: 1 })
    );
    assert_eq!(
        Toc::new(vec![audio(1, 100), audio(2, 100)], 1000),
        Err(TocError::NotAscending { index: 1 })
    );
    assert_eq!(
        Toc::new(vec![audio(1, 0), audio(2, 1000)], 1000),
        Err(TocError::LeadoutNotAfterLastTrack)
    );
    assert_eq!(
        Toc::new(vec![data(1, 0)], 1000),
        Err(TocError::NoAudioTrack)
    );
    assert_eq!(
        Toc::new(vec![audio(0, 0)], 1000),
        Err(TocError::InvalidTrackNumber(0))
    );
    let too_many: Vec<TocTrack> = (1..=100).map(|i| audio(i, u32::from(i) * 1000)).collect();
    assert_eq!(
        Toc::new(too_many, 200_000),
        Err(TocError::TooManyTracks(100))
    );
    // Enhanced CD でデータトラックが最後の音声トラックに近すぎる（終端が開始より前になる）
    assert_eq!(
        Toc::new(vec![audio(1, 0), audio(2, 5000), data(3, 16400)], 50_000),
        Err(TocError::SessionGapTooSmall { index: 2 })
    );
    assert!(Toc::new(vec![audio(1, 0), audio(2, 5000), data(3, 16401)], 50_000).is_ok());
}

/// 先頭のトラック番号は 1（フルディスクの TOC を表す型。MB / CTDB も first=1 前提）
#[test]
fn toc_rejects_a_first_track_other_than_one() {
    assert_eq!(
        Toc::new(vec![audio(2, 0), audio(3, 100)], 1000),
        Err(TocError::FirstTrackNotOne(2))
    );
}

/// 音声トラックは連続していること（data*→audio* の Mixed Mode と audio*→data* の Enhanced CD、
/// その組み合わせだけ）。audio→data→audio は拒否する
#[test]
fn toc_rejects_non_contiguous_audio_tracks() {
    assert_eq!(
        Toc::new(vec![audio(1, 0), data(2, 20000), audio(3, 40000)], 60000),
        Err(TocError::AudioTracksNotContiguous { index: 1 })
    );
    let both = Toc::new(vec![data(1, 0), audio(2, 20000), data(3, 60000)], 80000)
        .expect("data→audio→data");
    assert_eq!(both.audio_session().leadout_lba(), 60000 - 11400);
    assert_eq!(both.first_track(), 1);
}

/// セッション間隙の判定は差で行う（和は u32 を溢れる）
#[test]
fn session_gap_check_does_not_overflow_near_u32_max() {
    let toc = Toc::new(
        vec![
            audio(1, 0),
            audio(2, u32::MAX - 300),
            data(3, u32::MAX - 250),
        ],
        u32::MAX - 150,
    );
    assert_eq!(toc, Err(TocError::SessionGapTooSmall { index: 2 }));
}

/// MB / FreeDB のオフセット（+150）が u32 に収まらない TOC は受理しない。収まる上限では計算できる
#[test]
fn toc_rejects_a_leadout_that_cannot_take_the_pregap_offset() {
    assert_eq!(
        Toc::new(vec![audio(1, u32::MAX - 300)], u32::MAX - 100),
        Err(TocError::TooLong)
    );
    let edge = Toc::new(vec![audio(1, u32::MAX - 300)], u32::MAX - 150).expect("上限ちょうど");
    assert_eq!(edge.musicbrainz_disc_id().len(), 28);
    assert_eq!(edge.accuraterip_id().audio_tracks, 1);
}

// ---------------------------------------------------------------- 文字列からの読み取り

/// CTDB 形式は ctdb_toc() の逆。データトラックの `-` も戻る
#[test]
fn ctdb_toc_string_round_trips() {
    for toc in [
        mb_doc_disc(),
        nevermind(),
        hybrid_theory(),
        mb_doc_cd_extra(),
    ] {
        assert_eq!(Toc::parse(&toc.ctdb_toc()).expect("parse"), toc);
    }
    assert_eq!(
        Toc::parse(" 0 : 15213 :32164:46442:63264:80339:95312 ").expect("空白は許す"),
        mb_doc_disc()
    );
}

/// MusicBrainz 形式は musicbrainz_toc() の逆（+150 を戻す。音声のみ）
#[test]
fn musicbrainz_toc_string_round_trips() {
    for toc in [mb_doc_disc(), nevermind()] {
        assert_eq!(Toc::parse(&toc.musicbrainz_toc()).expect("parse"), toc);
    }
    // CD-Extra は音声セッションに戻る
    assert_eq!(
        Toc::parse(&mb_doc_cd_extra().musicbrainz_toc()).expect("parse"),
        mb_doc_cd_extra().audio_session()
    );
}

#[test]
fn toc_string_parsing_rejects_garbage() {
    for bad in [
        "",
        "abc",
        "0:10:5:20",        // 昇順でない
        "0:100",            // 1 トラックの CTDB 形式は OK なので次で確かめる
        "1 2",              // MB 形式が短い
        "1 3 1000 150 300", // オフセットの数が合わない
        "1 2 1000 100 300", // 150 未満
        "0 1 1000 150 300", // 先頭が 0
        "1:2:x",
    ] {
        let r = Toc::parse(bad);
        if bad == "0:100" {
            assert!(r.is_ok(), "{bad}");
        } else {
            assert!(r.is_err(), "{bad}: {r:?}");
        }
    }
}
