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
    // i32::MIN も溢れずに InvalidInput
    let e = shift_pcm(&src, &dst, i32::MIN).unwrap_err();
    assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput);
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

// ---------------------------------------------------------------- cd-paranoia

use spindle::cd::rip::{paranoia_span, parse_paranoia_line, read_disc, ParanoiaEvent, RipError};
use tokio_util::sync::CancellationToken;

#[test]
fn paranoia_lines_and_span() {
    assert_eq!(
        parse_paranoia_line("##: 14 [wrote] @ 1175"),
        Some(ParanoiaEvent {
            code: 14,
            pos: Some(1175)
        })
    );
    assert_eq!(
        parse_paranoia_line("##: 0 [read]"),
        Some(ParanoiaEvent { code: 0, pos: None })
    );
    assert_eq!(parse_paranoia_line("Ripping from sector 0"), None);
    assert_eq!(parse_paranoia_line("##: x [y] @ 1"), None);
    // 最後のトラックは TOC の長さの最後のセクタまで（含む）。900 セクタ = 12 秒 → 11.74
    assert_eq!(paranoia_span(&toc()).unwrap(), "1-3[0:11.74]");
    // Enhanced CD: データトラックは読まず、音声の終端は データ開始 − 11400
    let enhanced = Toc::parse("0:20000:-60000:80000").unwrap();
    let last = enhanced.audio_track_sectors().last().unwrap().1;
    assert_eq!(last, 60000 - 11400 - 20000);
    assert!(paranoia_span(&enhanced).unwrap().starts_with("1-2["));
}

/// 偽の cd-paranoia（`tests/fixtures/fake-paranoia.sh`）の振る舞いを `dir`（出力先と同じディレクトリ）に
/// 書く: `bytes` バイトの PCM を書き、`stderr` を出して `code` で終わる。引数は `dir/args` に残る
fn fake_paranoia(dir: &std::path::Path, bytes: u64, stderr: &str, code: i32) -> std::path::PathBuf {
    std::fs::write(
        dir.join("fake.conf"),
        format!("BYTES={bytes}\nCODE={code}\n"),
    )
    .unwrap();
    std::fs::write(dir.join("fake.stderr"), stderr).unwrap();
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-paranoia.sh")
}

#[tokio::test]
async fn read_disc_reports_progress_and_per_track_trouble() {
    let t = toc(); // 750 / 600 / 900 セクタ
    let total_bytes = t.track_layout().unwrap().total_samples() * 4;
    let dir = tempfile::tempdir().unwrap();
    let stderr = [
        "Ripping from sector 0",
        "##: 0 [read] @ 19992",
        "##: 14 [wrote] @ 1175",
        "##: 12 [read_error] @ 1000", // セクタ 0 → トラック 1
        "##: 6 [skip] @ 900000",      // セクタ 765 → トラック 2
        "##: 4 [scratch] @ 1500000",  // セクタ 1275 → トラック 2
        "##: 5 [repair] @ 2000000",   // セクタ 1700 → トラック 3
        "##: 2 [jitter] @ 2000000",   // 数えない（ずれを直せたもの）
        "##: 7 [drift] @ 1000",       // 位置を見失った → トラック 1 のずれ
        "##: 10 [dropped] @ 900000",  // 補正でデータを捨てた → トラック 2
        "##: 11 [duped] @ 2000000",   // 補正で重複を消した → トラック 3
        "##: 7 [drift] @ 2000000",
        "##: 14 [wrote] @ 2645999", // 2250 セクタ目の終わり
        "##: 15 [finished] @ 2645999",
    ]
    .join("\n");
    let prog = fake_paranoia(dir.path(), total_bytes, &stderr, 0);
    let out = dir.path().join("disc.pcm");
    let mut seen = Vec::new();
    let reads = read_disc(
        &prog,
        std::path::Path::new("/dev/sr0"),
        &t,
        &out,
        |d, n| seen.push((d, n)),
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(
        reads.iter().map(|r| r.rereads).collect::<Vec<_>>(),
        [1, 2, 1]
    );
    // ドライブのジッターを paranoia が検出・補正した回数（drift / dropped / duped）は別に数える
    assert_eq!(reads.iter().map(|r| r.slips).collect::<Vec<_>>(), [1, 1, 2]);
    assert_eq!(seen, [(1, 2250), (2250, 2250)]);
    let args = std::fs::read_to_string(dir.path().join("args")).unwrap();
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        [
            "-e",
            "-r",
            "-d",
            "/dev/sr0",
            "--",
            "1-3[0:11.74]",
            out.to_str().unwrap()
        ]
    );
}

#[tokio::test]
async fn read_disc_fails_on_short_output_and_exit_status() {
    let t = toc();
    let total_bytes = t.track_layout().unwrap().total_samples() * 4;
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("disc.pcm");
    let short = fake_paranoia(dir.path(), total_bytes - 4, "", 0);
    assert!(matches!(
        read_disc(
            &short,
            std::path::Path::new("/dev/sr0"),
            &t,
            &out,
            |_, _| {},
            &CancellationToken::new()
        )
        .await,
        Err(RipError::Length { .. })
    ));
    let failing = fake_paranoia(dir.path(), total_bytes, "scsi read error", 1);
    let e = read_disc(
        &failing,
        std::path::Path::new("/dev/sr0"),
        &t,
        &out,
        |_, _| {},
        &CancellationToken::new(),
    )
    .await
    .unwrap_err();
    assert!(matches!(e, RipError::Process(_)), "{e}");
    assert!(e.to_string().contains("scsi read error"), "{e}");
}

/// 実ドライブ: 入っている盤を丸ごと吸い、CTDB / AccurateRip と照合して見つかったオフセットを出す。
/// `sudo -u ubuntu -g cdrom target/debug/deps/cd_rip-* --ignored --exact real_drive_reads_and_detects_offset --nocapture`
#[tokio::test]
#[ignore]
async fn real_drive_reads_and_detects_offset() {
    use spindle::cd::accuraterip::AccurateRipClient;
    use spindle::cd::ctdb::CtdbClient;
    use spindle::cd::device::{Drive, LinuxDrive};
    use spindle::cd::verify::{match_accuraterip, match_ctdb};
    let dev = std::env::var("SPINDLE_TEST_CD_DEVICE").unwrap_or_else(|_| "/dev/sr0".into());
    let drive = LinuxDrive::new(dev.clone().into());
    let t = drive.read_toc().unwrap();
    eprintln!("model = {:?}", drive.model().unwrap());
    eprintln!("toc = {}", t.ctdb_toc());
    let dir = tempfile::tempdir().unwrap();
    let out = dir.path().join("disc.pcm");
    let started = std::time::Instant::now();
    let mut last = 0;
    let reads = read_disc(
        std::path::Path::new("cd-paranoia"),
        std::path::Path::new(&dev),
        &t,
        &out,
        |d, n| {
            if d * 10 / n != last {
                last = d * 10 / n;
                eprintln!("progress {d}/{n} ({:?})", started.elapsed());
            }
        },
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    eprintln!("reads = {reads:?} in {:?}", started.elapsed());
    if let Ok(keep) = std::env::var("SPINDLE_TEST_KEEP_PCM") {
        std::fs::copy(&out, &keep).unwrap();
        eprintln!("kept pcm at {keep}");
    }
    let pcm = from_bytes(&std::fs::read(&out).unwrap());
    let mine = table(&t, &pcm);
    let ua = "spindle-test/0 ( https://github.com/AkashiSN/spindle )";
    let ctdb = CtdbClient::new("http://db.cuetools.net/lookup2.php", ua).unwrap();
    let ar = AccurateRipClient::new("http://www.accuraterip.com/accuraterip/", ua).unwrap();
    match ctdb.lookup(&t).await {
        Ok(entries) => {
            let m = match_ctdb(&mine, &t, &entries);
            eprintln!(
                "ctdb: {} entries → {:?} offset {} conf {}",
                entries.len(),
                m.outcome,
                m.offset,
                m.confidence
            );
            for e in &entries {
                eprintln!(
                    "  entry id {} conf {} toc {} npar {} parity {:?} syndrome {}",
                    e.id,
                    e.confidence,
                    e.toc,
                    e.npar,
                    e.has_parity.is_some(),
                    e.syndrome.is_some()
                );
                // シンドロームの列 0 でオフセットを探す（誤りがあっても見つかる）
                use spindle::cd::repair::{decode_entry_syndrome, SyndromeSampler};
                if let Ok(Some(col0)) = decode_entry_syndrome(e) {
                    let npar = col0.len();
                    let frames = pcm.len() as u64 / 2;
                    let mut s = SyndromeSampler::new(frames, npar).unwrap();
                    for chunk in pcm.chunks(1 << 16) {
                        s.push(chunk).unwrap();
                    }
                    let syn = s.finish().unwrap();
                    eprintln!("  find_offset = {:?}", syn.find_offset(&col0, MAX_OFFSET));
                }
            }
        }
        Err(e) => eprintln!("ctdb lookup failed: {e}"),
    }
    match ar.lookup(&t.accuraterip_id()).await {
        Ok(entries) => {
            let m = match_accuraterip(&mine, &entries);
            eprintln!(
                "ar: {} entries → {:?} offset {} conf {}",
                entries.len(),
                m.outcome,
                m.offset,
                m.confidence
            );
        }
        Err(e) => eprintln!("ar lookup failed: {e}"),
    }
}
