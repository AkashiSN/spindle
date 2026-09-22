//! 吸い出しの部品（`cd::rip`。SPEC §7.2、D-83、P2-5）

use spindle::cd::crctable::{CrcSampler, CrcTable, MAX_OFFSET};
use spindle::cd::rip::shift_pcm;
use spindle::cd::toc::Toc;

const SECTOR: u64 = 588;

fn toc() -> Toc {
    Toc::from_audio_sample_counts([750 * SECTOR, 600 * SECTOR, 900 * SECTOR]).unwrap()
}

/// 決定的な擬似乱数のインターリーブ i16
fn samples(frames: u64) -> Vec<i16> {
    let mut x: u32 = 0x1234_5678;
    (0..frames * 2)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as i16
        })
        .collect()
}

fn table(t: &Toc, pcm: &[i16]) -> CrcTable {
    let mut s = CrcSampler::new(&t.track_layout().unwrap());
    for chunk in pcm.chunks(8192) {
        s.push(chunk).unwrap();
    }
    s.finish().unwrap()
}

fn to_bytes(pcm: &[i16]) -> Vec<u8> {
    pcm.iter().flat_map(|s| s.to_le_bytes()).collect()
}

fn from_bytes(b: &[u8]) -> Vec<i16> {
    b.chunks(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect()
}

/// 照合で見つかるオフセットの向きと、それを当てる向きが合っていること: `d` サンプル遅れて
/// 読めたデータの表で DB（正しいデータ）の CRC を探すと `d` が見つかり、`shift_pcm(d)` で
/// 全トラックの CRC がオフセット 0 で一致する
#[test]
fn detected_offset_applied_by_shift_pcm_restores_the_crcs() {
    let t = toc();
    let frames = t.track_layout().unwrap().total_samples();
    let truth = samples(frames);
    let db = table(&t, &truth);
    for d in [667i32, -667, 1, -1, MAX_OFFSET, -MAX_OFFSET] {
        // ドライブのずれで `d` サンプル遅れた（負なら早い）データ
        let n = truth.len();
        let k = (d.unsigned_abs() as usize) * 2;
        let ours: Vec<i16> = if d > 0 {
            std::iter::repeat_n(0, k)
                .chain(truth[..n - k].iter().copied())
                .collect()
        } else {
            truth[k..]
                .iter()
                .copied()
                .chain(std::iter::repeat_n(0, k))
                .collect()
        };
        let mine = table(&t, &ours);
        // 真ん中のトラックの AR v1 で探す（照合の `choose` と同じ見方）
        let want = db.ar_v1(1, 0).unwrap();
        let found: Vec<i32> = (-MAX_OFFSET..=MAX_OFFSET)
            .filter(|&o| mine.ar_v1(1, o) == Some(want))
            .collect();
        assert_eq!(found, [d], "d = {d}");

        let dir = tempfile::tempdir().unwrap();
        let (src, dst) = (dir.path().join("a.pcm"), dir.path().join("b.pcm"));
        std::fs::write(&src, to_bytes(&ours)).unwrap();
        shift_pcm(&src, &dst, d).unwrap();
        let fixed = from_bytes(&std::fs::read(&dst).unwrap());
        assert_eq!(fixed.len(), truth.len());
        let got = table(&t, &fixed);
        for track in 0..3 {
            assert_eq!(
                got.ar_v1(track, 0),
                db.ar_v1(track, 0),
                "d = {d} track {track}"
            );
            assert_eq!(got.ctdb_track(track, 0), db.ctdb_track(track, 0));
        }
        assert_eq!(got.ctdb_disc(0), db.ctdb_disc(0));
    }
}

#[test]
fn shift_pcm_rejects_out_of_range() {
    let dir = tempfile::tempdir().unwrap();
    let (src, dst) = (dir.path().join("a.pcm"), dir.path().join("b.pcm"));
    std::fs::write(&src, vec![1u8; 400]).unwrap();
    assert!(shift_pcm(&src, &dst, 101).is_err()); // 長さより大きい
    assert!(shift_pcm(&src, &dst, MAX_OFFSET + 1).is_err());
    shift_pcm(&src, &dst, 0).unwrap();
    assert_eq!(std::fs::read(&dst).unwrap(), vec![1u8; 400]);
    shift_pcm(&src, &dst, 2).unwrap();
    let b = std::fs::read(&dst).unwrap();
    assert_eq!((&b[..392], &b[392..]), (&vec![1u8; 392][..], &[0u8; 8][..]));
}

/// 学習したオフセットはドライブの型番ごとに 1 つで、最後に照合が通った値で置き換わる
#[test]
fn learned_offsets_are_per_drive_and_replaced() {
    use spindle::db::drive_offsets::{self, OffsetMethod};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("t.db");
    drop(spindle::db::Db::open(&path).unwrap());
    let c = rusqlite::Connection::open(&path).unwrap();
    let drive = "PIONEER BD-RW   BDR-209M";
    assert_eq!(drive_offsets::get(&c, drive).unwrap(), None);
    drive_offsets::set(&c, drive, 667, OffsetMethod::Ctdb, 12, 100).unwrap();
    drive_offsets::set(&c, "OTHER", -30, OffsetMethod::AccurateRip, 3, 100).unwrap();
    drive_offsets::set(&c, drive, 667, OffsetMethod::AccurateRip, 5, 200).unwrap();
    let got = drive_offsets::get(&c, drive).unwrap().unwrap();
    assert_eq!(
        (
            got.offset,
            got.method.as_str(),
            got.confidence,
            got.detected_at
        ),
        (667, "accuraterip", 5, 200)
    );
    assert_eq!(
        drive_offsets::get(&c, "OTHER").unwrap().unwrap().offset,
        -30
    );
}
