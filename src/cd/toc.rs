//! TOC と各種 DiscID（SPEC §7.2「ID 算出」、§7.3「TOC 再構成」、§15 `cd/toc.rs`）。
//! すべて TOC からの整数演算で、libdiscid の FFI は使わない。
//!
//! 位置は LBA（先頭トラックが 0。MSF の 00:02:00 に当たる 150 セクタの pregap は含まない）で持つ。
//! MusicBrainz と FreeDB は +150 したオフセット、AccurateRip と CTDB は LBA そのものを使う。
//!
//! Enhanced CD（音声セッションの後にデータトラックのセッション）では、音声部分の終端は
//! データトラック開始 − 11400（リードアウト / リードイン 11250 + pregap 150）。
//! - MusicBrainz DiscID と CTDB はデータトラックを数えず、この終端をリードアウトとして使う
//!   （MusicBrainz「Disc ID Calculation」、CUETools `CDImageLayout`）
//! - AccurateRip ID（dBpoweramp / EAC / CUETools 式）は id1 / id2 に音声トラックだけを足すが、
//!   リードアウトは実際の値を使い、FreeDB ID はデータトラックも数える
//!   （CUETools `AccurateRipVerify.CalculateAccurateRipId` / `CalculateCDDBId`）。
//!   データトラックを落として −11400 で切った TOC（[`Toc::audio_session`]）から作る ID も
//!   AccurateRip DB に別キーとして存在する（Hybrid Theory JP 盤で両方を確認）
//!
//! TOC の読み取り（`cdrdao` / `READ TOC`）は P2-1 / P2-2 のドライブ側。ここは純粋な計算

use std::fmt;

use base64::Engine;
use sha1::{Digest, Sha1};

use super::{LayoutError, TrackLayout, SECTOR_SAMPLES};

/// 先頭トラック前の pregap（セクタ）。MSF 00:02:00
pub const PREGAP_SECTORS: u32 = 150;
/// 音声セッションとデータセッションの間隙（セクタ）。リードアウト 6750 + リードイン 4500 + pregap 150
pub const SESSION_GAP_SECTORS: u32 = 11400;
/// CD-DA のトラック数の上限
pub const MAX_TRACKS: usize = 99;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TocTrack {
    /// トラック番号（1..=99）
    pub number: u8,
    /// 開始位置（LBA）
    pub start_lba: u32,
    pub is_audio: bool,
}

/// ディスクの TOC。トラックは番号順で、リードアウトは最後のトラックの終端 + 1
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Toc {
    tracks: Vec<TocTrack>,
    leadout_lba: u32,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TocError {
    #[error("トラックが 1 本もない")]
    NoTracks,
    #[error("トラック数が多すぎる: {0}（上限 99）")]
    TooManyTracks(usize),
    #[error("トラック番号が範囲外: {0}（1..=99）")]
    InvalidTrackNumber(u8),
    #[error("先頭のトラック番号が 1 でない: {0}")]
    FirstTrackNotOne(u8),
    #[error("トラック番号が連続していない（{index} 番目）")]
    NonConsecutiveNumbers { index: usize },
    #[error("開始位置が昇順でない（{index} 番目）")]
    NotAscending { index: usize },
    #[error("リードアウトが最後のトラックの後にない")]
    LeadoutNotAfterLastTrack,
    #[error("音声トラックが 1 本もない")]
    NoAudioTrack,
    #[error("音声トラックが連続していない（{index} 番目が間に挟まったデータトラック）")]
    AudioTracksNotContiguous { index: usize },
    #[error("最後の音声トラックとデータトラックの間隔が 11400 セクタ未満（{index} 番目）")]
    SessionGapTooSmall { index: usize },
    #[error("ディスクが長すぎる（LBA が 32 bit に収まらない）")]
    TooLong,
    #[error("トラック {index} のサンプル数 {samples} が 588 の倍数でない（CD 由来でない）")]
    NotSectorAligned { index: usize, samples: u64 },
    #[error("TOC 文字列を解釈できない: {0}")]
    Unparsable(String),
}

/// AccurateRip の 3 つの ID。DB のパスは
/// `/accuraterip/{id1 の下位 4 bit}/{次の 4 bit}/{次の 4 bit}/dBAR-{音声トラック数:03}-{id1}-{id2}-{cddb}.bin`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccurateRipId {
    pub id1: u32,
    pub id2: u32,
    /// FreeDB ID と同じ
    pub cddb: u32,
    pub audio_tracks: u8,
}

impl fmt::Display for AccurateRipId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:08x}-{:08x}-{:08x}", self.id1, self.id2, self.cddb)
    }
}

/// MusicBrainz / CTDB の base64: `+` → `.`、`/` → `_`、`=` → `-`
fn mb_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD
        .encode(bytes)
        .replace('+', ".")
        .replace('/', "_")
        .replace('=', "-")
}

fn sha1_hex_string(s: &str) -> String {
    mb_base64(&Sha1::digest(s.as_bytes()))
}

/// FreeDB の桁和（秒を 10 進で書いた各桁の和）
fn digit_sum(mut n: u32) -> u32 {
    let mut sum = 0;
    while n > 0 {
        sum += n % 10;
        n /= 10;
    }
    sum
}

impl Toc {
    /// 検証して作る。番号は 1 から連続、開始位置は昇順、リードアウトは最後のトラックより後で
    /// +150 のオフセットが u32 に収まる、音声トラックが 1 本以上で連続（data*→audio*→data*）、
    /// Enhanced CD ならデータトラックまでの間隙が 11400 セクタより大きい
    pub fn new(tracks: Vec<TocTrack>, leadout_lba: u32) -> Result<Self, TocError> {
        if tracks.is_empty() {
            return Err(TocError::NoTracks);
        }
        if tracks.len() > MAX_TRACKS {
            return Err(TocError::TooManyTracks(tracks.len()));
        }
        for (i, t) in tracks.iter().enumerate() {
            if !(1..=MAX_TRACKS as u8).contains(&t.number) {
                return Err(TocError::InvalidTrackNumber(t.number));
            }
            if i > 0 {
                let prev = &tracks[i - 1];
                if t.number != prev.number + 1 {
                    return Err(TocError::NonConsecutiveNumbers { index: i });
                }
                if t.start_lba <= prev.start_lba {
                    return Err(TocError::NotAscending { index: i });
                }
            }
        }
        if tracks[0].number != 1 {
            return Err(TocError::FirstTrackNotOne(tracks[0].number));
        }
        let last = &tracks[tracks.len() - 1];
        if leadout_lba <= last.start_lba {
            return Err(TocError::LeadoutNotAfterLastTrack);
        }
        // MusicBrainz / FreeDB のオフセット（+150）が u32 に収まること。開始位置はリードアウト未満
        if leadout_lba.checked_add(PREGAP_SECTORS).is_none() {
            return Err(TocError::TooLong);
        }
        let Some(first_audio) = tracks.iter().position(|t| t.is_audio) else {
            return Err(TocError::NoAudioTrack);
        };
        let last_audio = tracks
            .iter()
            .rposition(|t| t.is_audio)
            .unwrap_or(first_audio);
        if let Some(gap) = tracks[first_audio..=last_audio]
            .iter()
            .position(|t| !t.is_audio)
        {
            return Err(TocError::AudioTracksNotContiguous {
                index: first_audio + gap,
            });
        }
        // Enhanced CD: 最後の音声トラックの終端はデータトラック開始 − 11400 で、開始より後にあること。
        // 昇順は確認済みなので差で判定する（和は u32 を溢れる）
        if let Some(data) = tracks.get(last_audio + 1) {
            if data.start_lba - tracks[last_audio].start_lba <= SESSION_GAP_SECTORS {
                return Err(TocError::SessionGapTooSmall {
                    index: last_audio + 1,
                });
            }
        }
        Ok(Self {
            tracks,
            leadout_lba,
        })
    }

    /// §7.3: 各トラックのサンプル数（44.1 kHz / 16 bit / 2ch）から再構成する。
    /// 先頭は LBA 0、以降は前トラックのセクタ数の累積。588 の倍数でなければ CD 由来でないとして拒否
    pub fn from_audio_sample_counts(
        counts: impl IntoIterator<Item = u64>,
    ) -> Result<Self, TocError> {
        let mut tracks = Vec::new();
        let mut lba: u64 = 0;
        for (index, samples) in counts.into_iter().enumerate() {
            if samples == 0 || samples % SECTOR_SAMPLES != 0 {
                return Err(TocError::NotSectorAligned { index, samples });
            }
            if index >= MAX_TRACKS {
                return Err(TocError::TooManyTracks(index + 1));
            }
            tracks.push(TocTrack {
                number: index as u8 + 1,
                start_lba: u32::try_from(lba).map_err(|_| TocError::TooLong)?,
                is_audio: true,
            });
            lba += samples / SECTOR_SAMPLES;
        }
        let leadout = u32::try_from(lba).map_err(|_| TocError::TooLong)?;
        Self::new(tracks, leadout)
    }

    /// TOC 文字列を読む。CTDB 形式 `start:start:…:-datastart:leadout`（LBA。データトラックは
    /// `-` 前置。[`Toc::ctdb_toc`] の逆）か、MusicBrainz 形式 `first last leadout+150 offset+150 …`
    /// （[`Toc::musicbrainz_toc`] の逆。全部音声トラック）。どちらかは区切り（`:` か空白）で見分ける
    pub fn parse(s: &str) -> Result<Self, TocError> {
        let s = s.trim();
        if s.is_empty() {
            return Err(TocError::Unparsable("空".into()));
        }
        let num = |t: &str| -> Result<u32, TocError> {
            t.parse()
                .map_err(|_| TocError::Unparsable(format!("数でない: {t:?}")))
        };
        if s.contains(':') {
            let parts: Vec<&str> = s.split(':').map(str::trim).collect();
            let (&leadout, starts) = parts
                .split_last()
                .ok_or_else(|| TocError::Unparsable("短すぎる".into()))?;
            if starts.is_empty() {
                return Err(TocError::Unparsable("トラックが無い".into()));
            }
            let mut tracks = Vec::with_capacity(starts.len());
            for (i, t) in starts.iter().enumerate() {
                let (is_audio, t) = match t.strip_prefix('-') {
                    Some(data) => (false, data),
                    None => (true, *t),
                };
                tracks.push(TocTrack {
                    number: u8::try_from(i + 1)
                        .map_err(|_| TocError::TooManyTracks(starts.len()))?,
                    start_lba: num(t)?,
                    is_audio,
                });
            }
            return Self::new(tracks, num(leadout)?);
        }
        let parts: Vec<&str> = s.split_whitespace().collect();
        if parts.len() < 4 {
            return Err(TocError::Unparsable(
                "MusicBrainz 形式は「先頭 末尾 リードアウト オフセット…」".into(),
            ));
        }
        let first = num(parts[0])?;
        let last = num(parts[1])?;
        let leadout = num(parts[2])?;
        let offsets = &parts[3..];
        if first == 0 || last < first || last > MAX_TRACKS as u32 {
            return Err(TocError::Unparsable(format!(
                "トラック番号の範囲が不正: {first}..={last}"
            )));
        }
        if offsets.len() != (last - first + 1) as usize {
            return Err(TocError::Unparsable(format!(
                "オフセットの数 {} がトラック数 {} と合わない",
                offsets.len(),
                last - first + 1
            )));
        }
        let lba = |v: u32| -> Result<u32, TocError> {
            v.checked_sub(PREGAP_SECTORS)
                .ok_or_else(|| TocError::Unparsable(format!("オフセットが 150 未満: {v}")))
        };
        let tracks = offsets
            .iter()
            .enumerate()
            .map(|(i, o)| {
                Ok(TocTrack {
                    number: first as u8 + i as u8,
                    start_lba: lba(num(o)?)?,
                    is_audio: true,
                })
            })
            .collect::<Result<Vec<_>, TocError>>()?;
        Self::new(tracks, lba(leadout)?)
    }

    pub fn tracks(&self) -> &[TocTrack] {
        &self.tracks
    }

    pub fn leadout_lba(&self) -> u32 {
        self.leadout_lba
    }

    pub fn first_track(&self) -> u8 {
        self.tracks[0].number
    }

    pub fn last_track(&self) -> u8 {
        self.tracks[self.tracks.len() - 1].number
    }

    pub fn audio_tracks(&self) -> impl Iterator<Item = &TocTrack> {
        self.tracks.iter().filter(|t| t.is_audio)
    }

    /// 末尾のデータトラックがあれば、それを落としてリードアウトを「データトラック開始 − 11400」に
    /// した TOC。無ければ同じもの。先頭側のデータトラック（Mixed Mode CD）は残す
    pub fn audio_session(&self) -> Toc {
        let mut tracks = self.tracks.clone();
        let mut leadout = self.leadout_lba;
        // 末尾から連続するデータトラックを落とす。最後に落としたものの開始が新しい終端
        while tracks.len() > 1 && tracks.last().is_some_and(|t| !t.is_audio) {
            if let Some(dropped) = tracks.pop() {
                leadout = dropped.start_lba.saturating_sub(SESSION_GAP_SECTORS);
            }
        }
        Toc {
            tracks,
            leadout_lba: leadout,
        }
    }

    /// 音声部分の終端（LBA、排他的）。Enhanced CD ならデータトラック開始 − 11400
    pub fn audio_end_lba(&self) -> u32 {
        self.audio_session().leadout_lba
    }

    /// トラックの終端（LBA、排他的）。次のトラックの開始、最後ならリードアウト。
    /// 最後の音声トラックの次がデータトラックならセッション間隙を引く（CUETools と同じ）
    fn track_end_lba(&self, index: usize) -> u32 {
        let last_audio = self.tracks.iter().rposition(|t| t.is_audio);
        match self.tracks.get(index + 1) {
            Some(next) if !next.is_audio && last_audio == Some(index) => {
                next.start_lba.saturating_sub(SESSION_GAP_SECTORS)
            }
            Some(next) => next.start_lba,
            None => self.leadout_lba,
        }
    }

    /// CRC 計算用のレイアウト（音声トラックだけ、サンプル単位）
    pub fn track_layout(&self) -> Result<TrackLayout, LayoutError> {
        let lengths = self
            .tracks
            .iter()
            .enumerate()
            .filter(|(_, t)| t.is_audio)
            .map(|(i, t)| u64::from(self.track_end_lba(i) - t.start_lba) * SECTOR_SAMPLES);
        TrackLayout::from_sample_counts(lengths)
    }

    // ------------------------------------------------------------ MusicBrainz

    /// MusicBrainz DiscID。先頭・末尾トラック番号、リードアウト、各トラックのオフセット（+150）を
    /// 16 進大文字で並べた SHA-1 を base64（`.` `_` `-`）にしたもの。末尾のデータトラックは数えない
    pub fn musicbrainz_disc_id(&self) -> String {
        let session = self.audio_session();
        let mut s = format!("{:02X}{:02X}", session.first_track(), session.last_track());
        // offsets[0] がリードアウト、offsets[n] がトラック n。無い番号は 0
        let mut offsets = [0u32; 100];
        offsets[0] = session.leadout_lba + PREGAP_SECTORS;
        for t in &session.tracks {
            offsets[usize::from(t.number)] = t.start_lba + PREGAP_SECTORS;
        }
        for o in offsets {
            s.push_str(&format!("{o:08X}"));
        }
        sha1_hex_string(&s)
    }

    /// `ws/2/discid/-?toc=` に渡す TOC（先頭・末尾トラック番号、リードアウト、各オフセット。+150 済み）
    pub fn musicbrainz_toc(&self) -> String {
        let session = self.audio_session();
        let mut parts = vec![
            session.first_track().to_string(),
            session.last_track().to_string(),
            (session.leadout_lba + PREGAP_SECTORS).to_string(),
        ];
        parts.extend(
            session
                .tracks
                .iter()
                .map(|t| (t.start_lba + PREGAP_SECTORS).to_string()),
        );
        parts.join(" ")
    }

    // ------------------------------------------------------------ FreeDB / AccurateRip

    /// FreeDB（CDDB）ID。全トラック（データトラック含む）の開始秒の桁和、総秒数、トラック数から
    pub fn freedb_id(&self) -> u32 {
        let n: u32 = self
            .tracks
            .iter()
            .map(|t| digit_sum(t.start_lba / 75 + 2))
            .sum();
        let total_seconds = self.leadout_lba / 75 - self.tracks[0].start_lba / 75;
        ((n % 255) << 24) | (total_seconds << 8) | self.tracks.len() as u32
    }

    /// AccurateRip の ID（dBpoweramp / EAC / CUETools 式）。id1 は音声トラックの開始 LBA の和 +
    /// リードアウト、id2 は各開始 LBA（0 なら 1）× 音声トラック内の連番の和 + リードアウト × (n+1)
    pub fn accuraterip_id(&self) -> AccurateRipId {
        let mut id1: u32 = 0;
        let mut id2: u32 = 0;
        let mut n: u32 = 0;
        for t in self.audio_tracks() {
            n += 1;
            id1 = id1.wrapping_add(t.start_lba);
            id2 = id2.wrapping_add(t.start_lba.max(1).wrapping_mul(n));
        }
        id1 = id1.wrapping_add(self.leadout_lba);
        id2 = id2.wrapping_add(self.leadout_lba.max(1).wrapping_mul(n + 1));
        AccurateRipId {
            id1,
            id2,
            cddb: self.freedb_id(),
            audio_tracks: n as u8,
        }
    }

    // ------------------------------------------------------------ CTDB

    /// CTDB の照会に使う TOC 文字列。各トラックの開始 LBA（データトラックは `-` 前置）と
    /// リードアウトを `:` で繋ぐ（CUETools `CDImageLayout.ToString`）
    pub fn ctdb_toc(&self) -> String {
        let mut parts: Vec<String> = self
            .tracks
            .iter()
            .map(|t| {
                if t.is_audio {
                    t.start_lba.to_string()
                } else {
                    format!("-{}", t.start_lba)
                }
            })
            .collect();
        parts.push(self.leadout_lba.to_string());
        parts.join(":")
    }

    /// CTDB の TOCID。先頭の音声トラックからの相対開始位置（2 本目以降）と音声部分の長さを
    /// 16 進大文字で並べ、100 要素になるまで 0 で埋めた SHA-1（CUETools `CDImageLayout.TOCID`）
    pub fn ctdb_toc_id(&self) -> String {
        let audio: Vec<&TocTrack> = self.audio_tracks().collect();
        let first = audio[0].start_lba;
        let mut s = String::new();
        for t in &audio[1..] {
            s.push_str(&format!("{:08X}", t.start_lba - first));
        }
        s.push_str(&format!("{:08X}", self.audio_end_lba() - first));
        for _ in audio.len()..100 {
            s.push_str("00000000");
        }
        sha1_hex_string(&s)
    }
}
