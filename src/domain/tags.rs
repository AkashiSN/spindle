//! タグ集合の正規化とハッシュ（SPEC §6「変更検出と版の遷移」）。
//!
//! `tag_version` は `tag_hash` の実差分があるときだけ進める。同じ内容の再保存（外部ツールの
//! 同値保存、tagwrite の再実行）で Derived の追随が空回りしないよう、表記差（キーの大小文字、
//! 値の NFC / NFD、キーの並び）を吸収してからハッシュする。lofty からの読み取りは P0-6。

use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// 埋め込み画像を表す擬似キー。カバー差し替えも `tag_version` を進める（Derived のタグ追随対象）
const PICTURE_KEY: &str = "PICTURE";

/// 正規化済みのタグ集合。キーは大文字、値は NFC、キー順に整列（同一キーの多値は入力順を保持）
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TagSet {
    items: Vec<(String, String)>,
}

impl TagSet {
    pub fn items(&self) -> &[(String, String)] {
        &self.items
    }

    /// 埋め込み画像をバイト列のハッシュとして加える。画像そのものは保持しない
    pub fn add_picture(&mut self, mime: &str, data: &[u8]) {
        let digest = Sha256::digest(data);
        let value = format!("{mime}:{}", hex(&digest));
        self.insert(PICTURE_KEY.to_owned(), value);
    }

    /// `(key, value)` を正規化して加える（キー大文字化・値 NFC、同一キーは入力順を保つ）
    pub fn extend<I>(&mut self, items: I)
    where
        I: IntoIterator<Item = (String, String)>,
    {
        for (key, value) in items {
            let key = key.to_uppercase();
            let value: String = value.nfc().collect();
            self.insert(key, value);
        }
    }

    /// 同一キーの値を順に返す
    pub fn values<'a>(&'a self, key: &str) -> impl Iterator<Item = &'a str> + 'a {
        let key = key.to_owned();
        self.items
            .iter()
            .filter(move |(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
    }

    /// 最初の値
    pub fn first(&self, key: &str) -> Option<&str> {
        self.items
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// キー順を保ったまま末尾（同一キーの最後）に挿入する
    fn insert(&mut self, key: String, value: String) {
        let pos = self
            .items
            .partition_point(|(k, _)| k.as_str() <= key.as_str());
        self.items.insert(pos, (key, value));
    }
}

/// `(key, value)` の列を正規化タグ集合にする
pub fn normalize_tags<I>(items: I) -> TagSet
where
    I: IntoIterator<Item = (String, String)>,
{
    let mut set = TagSet::default();
    set.extend(items);
    set
}

/// 正規化タグ集合の SHA-256。`KEY` と値をそれぞれ長さ前置で連結し、区切りの曖昧さを無くす
pub fn tag_hash(set: &TagSet) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for (key, value) in &set.items {
        for part in [key, value] {
            hasher.update((part.len() as u64).to_le_bytes());
            hasher.update(part.as_bytes());
        }
    }
    hasher.finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ---------------------------------------------------------------- lofty ラッパ

use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};

use lofty::config::ParseOptions;
use lofty::file::{AudioFile as _, FileType, TaggedFileExt};
use lofty::ogg::OggPictureStorage as _;
use lofty::probe::Probe;
use lofty::properties::FileProperties;
use lofty::tag::{ItemValue, TagType};

/// `tracks.codec` の値（スキーマの CHECK と一致させる）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Codec {
    Flac,
    Opus,
    Alac,
    Aac,
    Mp3,
    Wav,
    Ogg,
    Wv,
    Ape,
    Aiff,
}

impl Codec {
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::Flac => "flac",
            Codec::Opus => "opus",
            Codec::Alac => "alac",
            Codec::Aac => "aac",
            Codec::Mp3 => "mp3",
            Codec::Wav => "wav",
            Codec::Ogg => "ogg",
            Codec::Wv => "wv",
            Codec::Ape => "ape",
            Codec::Aiff => "aiff",
        }
    }

    pub fn parse(s: &str) -> Option<Codec> {
        [
            Codec::Flac,
            Codec::Opus,
            Codec::Alac,
            Codec::Aac,
            Codec::Mp3,
            Codec::Wav,
            Codec::Ogg,
            Codec::Wv,
            Codec::Ape,
            Codec::Aiff,
        ]
        .into_iter()
        .find(|c| c.as_str() == s)
    }

    /// 拡張子からの暫定判定（走査対象の判定に使う）。`m4a` は中身で ALAC / AAC が決まる
    pub fn from_extension(ext: &str) -> Option<Codec> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "flac" => Codec::Flac,
            "opus" => Codec::Opus,
            "m4a" | "mp4" | "aac" => Codec::Aac,
            "mp3" => Codec::Mp3,
            "wav" => Codec::Wav,
            "ogg" | "oga" => Codec::Ogg,
            "wv" => Codec::Wv,
            "ape" => Codec::Ape,
            "aiff" | "aif" => Codec::Aiff,
            _ => return None,
        })
    }

    pub fn lossless(self) -> bool {
        matches!(
            self,
            Codec::Flac | Codec::Alac | Codec::Wav | Codec::Wv | Codec::Ape | Codec::Aiff
        )
    }
}

/// ファイルから読んだタグと音声属性
#[derive(Debug, Clone)]
pub struct AudioFile {
    pub codec: Codec,
    pub lossless: bool,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channels: Option<u32>,
    /// kbps
    pub bitrate: Option<u32>,
    pub duration_ms: Option<u64>,
    pub tags: TagSet,
}

#[derive(Debug, thiserror::Error)]
pub enum TagReadError {
    #[error("音声ファイルとして認識できない")]
    Unrecognized,
    #[error("対応していない形式: {0:?}")]
    Unsupported(FileType),
    #[error("タグの読み取りに失敗: {0}")]
    Parse(#[from] lofty::error::FileParseError),
    #[error("読み取りに失敗: {0}")]
    Io(#[from] std::io::Error),
}

/// ファイルのタグと音声属性を読む。キーは Vorbis Comment 名（大文字）に揃え、任意キーと
/// 多値を保つ。FLAC / Opus / Vorbis は VorbisComments を直接読む（lofty の generic `Tag` は
/// 未知のキーを落とす）。他の形式は lofty の `ItemKey` を Vorbis 名へ写像する
pub fn read_audio_file(file: File, ext: Option<&str>) -> Result<AudioFile, TagReadError> {
    let mut reader = BufReader::new(file);
    let mut probe = Probe::new(&mut reader);
    if let Some(ext) = ext {
        if let Some(ft) = FileType::from_ext(ext) {
            probe = probe.set_file_type(ft);
        }
    }
    let probe = probe.guess_file_type()?;
    let ty = probe.file_type().ok_or(TagReadError::Unrecognized)?;
    let reader = probe.into_inner();
    reader.seek(SeekFrom::Start(0))?;
    let opts = ParseOptions::new();

    let mut set = TagSet::default();
    let (codec, props): (Codec, FileProperties) = match ty {
        FileType::Flac => {
            let f = lofty::flac::FlacFile::read_from(reader, opts)?;
            if let Some(vc) = f.vorbis_comments() {
                collect_vorbis(&mut set, vc);
            }
            for (pic, _) in f.pictures() {
                set.add_picture(mime_of(pic), pic.data());
            }
            (Codec::Flac, (*f.properties()).into())
        }
        FileType::Opus => {
            let f = lofty::ogg::OpusFile::read_from(reader, opts)?;
            collect_vorbis(&mut set, f.vorbis_comments());
            (Codec::Opus, (*f.properties()).into())
        }
        FileType::Vorbis => {
            let f = lofty::ogg::VorbisFile::read_from(reader, opts)?;
            collect_vorbis(&mut set, f.vorbis_comments());
            (Codec::Ogg, (*f.properties()).into())
        }
        FileType::Mp4 => {
            let f = lofty::mp4::Mp4File::read_from(reader, opts)?;
            let codec = match f.properties().codec() {
                Some(lofty::mp4::Mp4Codec::ALAC) => Codec::Alac,
                Some(lofty::mp4::Mp4Codec::AAC) => Codec::Aac,
                Some(lofty::mp4::Mp4Codec::MP3) => Codec::Mp3,
                Some(lofty::mp4::Mp4Codec::FLAC) => Codec::Flac,
                _ => return Err(TagReadError::Unsupported(ty)),
            };
            if let Some(ilst) = f.ilst() {
                collect_generic(&mut set, &lofty::tag::Tag::from(ilst.clone()));
            }
            (codec, f.properties().clone().into())
        }
        FileType::Mpeg | FileType::Wav | FileType::WavPack | FileType::Ape | FileType::Aiff => {
            let tagged = Probe::new(reader).set_file_type(ty).read()?;
            let codec = match ty {
                FileType::Mpeg => Codec::Mp3,
                FileType::Wav => Codec::Wav,
                FileType::WavPack => Codec::Wv,
                FileType::Ape => Codec::Ape,
                _ => Codec::Aiff,
            };
            for tag in tagged.tags() {
                collect_generic(&mut set, tag);
            }
            (codec, tagged.properties().clone())
        }
        other => return Err(TagReadError::Unsupported(other)),
    };

    Ok(AudioFile {
        codec,
        lossless: codec.lossless(),
        sample_rate: props.sample_rate(),
        bit_depth: props.bit_depth().map(u32::from),
        channels: props.channels().map(u32::from),
        bitrate: props.audio_bitrate().or(props.overall_bitrate()),
        duration_ms: Some(props.duration().as_millis() as u64),
        tags: set,
    })
}

fn mime_of(pic: &lofty::picture::Picture) -> &str {
    pic.mime_type().map(|m| m.as_str()).unwrap_or("")
}

/// VorbisComments の全項目を raw のまま取り込む（キーは大文字化、値は NFC）
fn collect_vorbis(set: &mut TagSet, vc: &lofty::ogg::tag::VorbisComments) {
    let items = vc.items().map(|(k, v)| (k.to_owned(), v.to_owned()));
    set.extend(items);
    for (pic, _) in vc.pictures() {
        set.add_picture(mime_of(pic), pic.data());
    }
}

/// lofty の generic `Tag` を Vorbis Comment 名へ写像して取り込む。名前を持たない項目は落ちる
fn collect_generic(set: &mut TagSet, tag: &lofty::tag::Tag) {
    let items = tag.items().filter_map(|item| {
        let key = item.key().map_key(TagType::VorbisComments)?;
        let value = match item.value() {
            ItemValue::Text(s) | ItemValue::Locator(s) => s.clone(),
            ItemValue::Binary(_) => return None,
        };
        Some((key.to_owned(), value))
    });
    set.extend(items);
    for pic in tag.pictures() {
        set.add_picture(mime_of(pic), pic.data());
    }
}
