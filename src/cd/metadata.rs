//! 確定したディスクのメタデータ（SPEC §7.2、D-65 / D-67、P2-8）。web の確定フォーム
//! （`web/src/lib/cd.ts` の `DiscMetadata`）と同じ形を JSON で受け、検証し、タグに写す。
//! タグの写像は web の `albumTags` / `trackTags` と同じキー・同じ順（確定画面がタグ名で見せる
//! ものをそのまま書く）で、`TRACKTOTAL` と `MUSICBRAINZ_DISCID` を TOC から足す。
//! `category` は配置先（`[layout]` の `{category}`）にだけ使い、タグには書かない（SPEC §5）

use serde::{Deserialize, Serialize};

use super::toc::Toc;

/// 候補から持ち越す MusicBrainz のトラック識別子
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrackMbIds {
    /// recording → `MUSICBRAINZ_TRACKID`（Picard の写像。取り違えやすい）
    pub recording_id: String,
    /// リリース内の track → `MUSICBRAINZ_RELEASETRACKID`
    pub track_id: String,
    #[serde(default)]
    pub isrcs: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscTrackMetadata {
    /// TOC の音声トラック番号（1..=99）
    pub number: u8,
    pub title: String,
    /// 空ならアルバムアーティスト（D-65）
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub mb: Option<TrackMbIds>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MetadataSource {
    Musicbrainz,
    Manual,
}

/// 確定したディスクのメタデータ（ウィザードの出力。吸い出しと配置が同じ形を受ける）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscMetadata {
    pub source: MetadataSource,
    #[serde(default)]
    pub release_id: Option<String>,
    #[serde(default)]
    pub release_group_id: Option<String>,
    pub album: String,
    pub album_artist: String,
    /// `YYYY` / `YYYY-MM` / `YYYY-MM-DD`
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub catalog_number: Option<String>,
    #[serde(default)]
    pub barcode: Option<String>,
    pub disc_no: u8,
    pub disc_count: u8,
    /// 配置先の category（統制語彙の名前）。無ければ `_Unsorted`
    #[serde(default)]
    pub category: Option<String>,
    /// TOC の音声トラックと 1:1（番号順）
    pub tracks: Vec<DiscTrackMetadata>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum MetadataError {
    #[error("アルバム名が空")]
    EmptyAlbum,
    #[error("アルバムアーティストが空")]
    EmptyAlbumArtist,
    #[error("トラック数が TOC と合わない: 期待 {expected}、受信 {got}")]
    TrackCount { expected: usize, got: usize },
    #[error("{index} 番目のトラック番号が TOC と合わない: 期待 {expected}、受信 {got}")]
    TrackNumber { index: usize, expected: u8, got: u8 },
    #[error("トラック {number} のタイトルが空")]
    EmptyTitle { number: u8 },
    #[error("日付の形が不正: {0}（YYYY / YYYY-MM / YYYY-MM-DD）")]
    BadDate(String),
    #[error("ディスク番号が不正: {disc_no} / {disc_count}")]
    BadDisc { disc_no: u8, disc_count: u8 },
    #[error("category が空")]
    EmptyCategory,
}

/// `YYYY[-MM[-DD]]` か（MM は 01..=12、DD は 01..=31）
fn is_valid_date(s: &str) -> bool {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.is_empty() || parts.len() > 3 {
        return false;
    }
    let digits = |p: &str, n: usize| p.len() == n && p.bytes().all(|b| b.is_ascii_digit());
    if !digits(parts[0], 4) {
        return false;
    }
    if let Some(m) = parts.get(1) {
        if !digits(m, 2) || !(1..=12).contains(&m.parse::<u32>().unwrap_or(0)) {
            return false;
        }
    }
    if let Some(d) = parts.get(2) {
        if !digits(d, 2) || !(1..=31).contains(&d.parse::<u32>().unwrap_or(0)) {
            return false;
        }
    }
    true
}

/// 値が空（trim して空）なら落とす
fn push_opt(out: &mut Vec<(String, String)>, key: &str, value: Option<&str>) {
    if let Some(v) = value {
        let v = v.trim();
        if !v.is_empty() {
            out.push((key.to_owned(), v.to_owned()));
        }
    }
}

impl DiscMetadata {
    /// 確定の条件（D-65）: アルバム名・アルバムアーティスト・各トラックのタイトル。行は TOC の
    /// 音声トラックと 1:1 で番号が一致。日付は `YYYY[-MM[-DD]]` か無し。ディスク番号は 1 以上で
    /// 枚数以下。category は指定するなら空でない
    pub fn validate(&self, toc: &Toc) -> Result<(), MetadataError> {
        if self.album.trim().is_empty() {
            return Err(MetadataError::EmptyAlbum);
        }
        if self.album_artist.trim().is_empty() {
            return Err(MetadataError::EmptyAlbumArtist);
        }
        if self.disc_no == 0 || self.disc_count == 0 || self.disc_no > self.disc_count {
            return Err(MetadataError::BadDisc {
                disc_no: self.disc_no,
                disc_count: self.disc_count,
            });
        }
        if let Some(c) = &self.category {
            if c.trim().is_empty() {
                return Err(MetadataError::EmptyCategory);
            }
        }
        if let Some(d) = &self.date {
            if !is_valid_date(d) {
                return Err(MetadataError::BadDate(d.clone()));
            }
        }
        let numbers: Vec<u8> = toc.audio_tracks().map(|t| t.number).collect();
        if numbers.len() != self.tracks.len() {
            return Err(MetadataError::TrackCount {
                expected: numbers.len(),
                got: self.tracks.len(),
            });
        }
        for (index, (t, &expected)) in self.tracks.iter().zip(&numbers).enumerate() {
            if t.number != expected {
                return Err(MetadataError::TrackNumber {
                    index,
                    expected,
                    got: t.number,
                });
            }
            if t.title.trim().is_empty() {
                return Err(MetadataError::EmptyTitle { number: t.number });
            }
        }
        Ok(())
    }

    /// トラック `index` のアーティスト（空ならアルバムアーティスト。D-65）
    pub fn track_artist(&self, index: usize) -> &str {
        match self.tracks.get(index).map(|t| t.artist.trim()) {
            Some(a) if !a.is_empty() => a,
            _ => self.album_artist.trim(),
        }
    }

    /// 発売年（`date` の先頭 4 桁が数字のとき）
    pub fn year(&self) -> Option<String> {
        let d = self.date.as_deref()?;
        let y: String = d.chars().take(4).collect();
        (y.len() == 4 && y.chars().all(|c| c.is_ascii_digit())).then_some(y)
    }

    /// アルバム共通のタグ（web の `albumTags` と同じ順・同じキー）
    pub fn album_tags(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        push_opt(&mut out, "ALBUM", Some(&self.album));
        push_opt(&mut out, "ALBUMARTIST", Some(&self.album_artist));
        push_opt(&mut out, "DATE", self.date.as_deref());
        push_opt(&mut out, "LABEL", self.label.as_deref());
        push_opt(&mut out, "CATALOGNUMBER", self.catalog_number.as_deref());
        push_opt(&mut out, "BARCODE", self.barcode.as_deref());
        push_opt(&mut out, "DISCNUMBER", Some(&self.disc_no.to_string()));
        push_opt(&mut out, "DISCTOTAL", Some(&self.disc_count.to_string()));
        push_opt(&mut out, "MUSICBRAINZ_ALBUMID", self.release_id.as_deref());
        push_opt(
            &mut out,
            "MUSICBRAINZ_RELEASEGROUPID",
            self.release_group_id.as_deref(),
        );
        out
    }

    /// トラック `index` のタグ（web の `trackTags` と同じ。ISRC は 1 値 1 行）
    pub fn track_tags(&self, index: usize) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let Some(t) = self.tracks.get(index) else {
            return out;
        };
        push_opt(&mut out, "TRACKNUMBER", Some(&t.number.to_string()));
        push_opt(&mut out, "TITLE", Some(&t.title));
        push_opt(&mut out, "ARTIST", Some(self.track_artist(index)));
        if let Some(mb) = &t.mb {
            push_opt(&mut out, "MUSICBRAINZ_TRACKID", Some(&mb.recording_id));
            push_opt(&mut out, "MUSICBRAINZ_RELEASETRACKID", Some(&mb.track_id));
            for isrc in &mb.isrcs {
                push_opt(&mut out, "ISRC", Some(isrc));
            }
        }
        out
    }

    /// 書き込むタグの全体: `album_tags` + `track_tags` + `TRACKTOTAL`（TOC の音声トラック数）+
    /// `MUSICBRAINZ_DISCID`
    pub fn tags_for(&self, toc: &Toc, index: usize) -> Vec<(String, String)> {
        let mut out = self.album_tags();
        out.extend(self.track_tags(index));
        out.push((
            "TRACKTOTAL".to_owned(),
            toc.audio_tracks().count().to_string(),
        ));
        out.push(("MUSICBRAINZ_DISCID".to_owned(), toc.musicbrainz_disc_id()));
        out
    }
}
