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

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};

use lofty::config::{ParseOptions, WriteOptions};
use lofty::file::{AudioFile as _, FileType, TaggedFileExt};
use lofty::ogg::tag::VorbisComments;
use lofty::ogg::OggPictureStorage as _;
use lofty::probe::Probe;
use lofty::properties::FileProperties;
use lofty::tag::{ItemKey, ItemValue, Tag, TagItem, TagType};

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
    let (af, _pictures) = read_parts(file, ext)?;
    Ok(af)
}

/// [`read_audio_file`] に加えて埋め込み画像の実体も返す（画像の差し替えで、捨てる旧画像を
/// 退避するために使う。D-60）
pub fn read_audio_file_with_pictures(
    file: File,
    ext: Option<&str>,
) -> Result<(AudioFile, Vec<lofty::picture::Picture>), TagReadError> {
    read_parts(file, ext)
}

/// 形式をまたいで移すタグ（ロスレス正規化で元ファイルから FLAC へ写す。SPEC §7.4）。
/// 項目は [`TagSet`] と同じ正規化済みの Vorbis 名・値（`PICTURE` 疑似キーは含まない）、
/// 画像は実体ごと持つ
#[derive(Debug, Clone, Default)]
pub struct TransferTags {
    pub items: Vec<(String, String)>,
    pub pictures: Vec<lofty::picture::Picture>,
}

/// 元ファイルのタグと画像を、別形式へ書くために読む
pub fn read_transfer_tags(file: File, ext: Option<&str>) -> Result<TransferTags, TagReadError> {
    let (af, pictures) = read_parts(file, ext)?;
    let items = af
        .tags
        .items()
        .iter()
        .filter(|(k, _)| k != PICTURE_KEY)
        .cloned()
        .collect();
    Ok(TransferTags { items, pictures })
}

fn read_parts(
    file: File,
    ext: Option<&str>,
) -> Result<(AudioFile, Vec<lofty::picture::Picture>), TagReadError> {
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
    let mut pictures: Vec<lofty::picture::Picture> = Vec::new();
    let (codec, props): (Codec, FileProperties) = match ty {
        FileType::Flac => {
            let f = lofty::flac::FlacFile::read_from(reader, opts)?;
            if let Some(vc) = f.vorbis_comments() {
                collect_vorbis(&mut set, &mut pictures, vc);
            }
            for (pic, _) in f.pictures() {
                set.add_picture(mime_of(pic), pic.data());
                pictures.push(pic.clone());
            }
            (Codec::Flac, (*f.properties()).into())
        }
        FileType::Opus => {
            let f = lofty::ogg::OpusFile::read_from(reader, opts)?;
            collect_vorbis(&mut set, &mut pictures, f.vorbis_comments());
            (Codec::Opus, (*f.properties()).into())
        }
        FileType::Vorbis => {
            let f = lofty::ogg::VorbisFile::read_from(reader, opts)?;
            collect_vorbis(&mut set, &mut pictures, f.vorbis_comments());
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
                collect_generic(
                    &mut set,
                    &mut pictures,
                    &lofty::tag::Tag::from(ilst.clone()),
                    &mut HashSet::new(),
                );
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
            // 複数ブロック（ID3v2 + ID3v1 など）は primary を優先し、副ブロックからは
            // primary に無いキーだけを補う。単純に連結すると同じ値が多値として二重になる
            let primary = tagged.primary_tag_type();
            let mut seen: HashSet<String> = HashSet::new();
            let ordered = tagged
                .primary_tag()
                .into_iter()
                .chain(tagged.tags().iter().filter(|t| t.tag_type() != primary));
            for tag in ordered {
                collect_generic(&mut set, &mut pictures, tag, &mut seen);
            }
            (codec, tagged.properties().clone())
        }
        other => return Err(TagReadError::Unsupported(other)),
    };

    Ok((
        AudioFile {
            codec,
            lossless: codec.lossless(),
            sample_rate: props.sample_rate(),
            bit_depth: props.bit_depth().map(u32::from),
            channels: props.channels().map(u32::from),
            bitrate: props.audio_bitrate().or(props.overall_bitrate()),
            duration_ms: Some(props.duration().as_millis() as u64),
            tags: set,
        },
        pictures,
    ))
}

fn mime_of(pic: &lofty::picture::Picture) -> &str {
    pic.mime_type().map(|m| m.as_str()).unwrap_or("")
}

/// VorbisComments の全項目を raw のまま取り込む（キーは大文字化、値は NFC）
fn collect_vorbis(
    set: &mut TagSet,
    pictures: &mut Vec<lofty::picture::Picture>,
    vc: &lofty::ogg::tag::VorbisComments,
) {
    let items = vc.items().map(|(k, v)| (k.to_owned(), v.to_owned()));
    set.extend(items);
    for (pic, _) in vc.pictures() {
        set.add_picture(mime_of(pic), pic.data());
        pictures.push(pic.clone());
    }
}

/// lofty の generic `Tag` を Vorbis Comment 名へ写像して取り込む。名前を持たない項目は落ちる。
/// `seen` に既にあるキー（先に読んだブロックが持つキー）は取り込まず、このブロックで
/// 取り込んだキーを `seen` に加える
fn collect_generic(
    set: &mut TagSet,
    pictures: &mut Vec<lofty::picture::Picture>,
    tag: &lofty::tag::Tag,
    seen: &mut HashSet<String>,
) {
    let mut added: Vec<String> = Vec::new();
    let items: Vec<(String, String)> = tag
        .items()
        .filter_map(|item| {
            let key = item.key().map_key(TagType::VorbisComments)?.to_uppercase();
            if seen.contains(&key) {
                return None;
            }
            let value = match item.value() {
                ItemValue::Text(s) | ItemValue::Locator(s) => s.clone(),
                ItemValue::Binary(_) => return None,
            };
            added.push(key.clone());
            Some((key, value))
        })
        .collect();
    set.extend(items);
    seen.extend(added);
    if !tag.pictures().is_empty() && !seen.contains(PICTURE_KEY) {
        for pic in tag.pictures() {
            set.add_picture(mime_of(pic), pic.data());
            pictures.push(pic.clone());
        }
        seen.insert(PICTURE_KEY.to_owned());
    }
}

// ---------------------------------------------------------------- 書き込み

/// 1 キーの変更。`values` が `None` ならそのキーを削除する（空配列も削除と同義）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagChange {
    pub key: String,
    pub values: Option<Vec<String>>,
}

impl TagChange {
    /// キーを大文字化し、値を NFC に正規化する（[`TagSet`] と同じ規則）。空配列は `None` に畳む
    pub fn normalized(&self) -> TagChange {
        let values = self
            .values
            .as_ref()
            .filter(|v| !v.is_empty())
            .map(|v| v.iter().map(|s| s.nfc().collect()).collect());
        TagChange {
            key: self.key.to_uppercase(),
            values,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TagWriteError {
    #[error("音声ファイルとして認識できない")]
    Unrecognized,
    #[error("対応していない形式: {0:?}")]
    Unsupported(FileType),
    #[error("タグの読み取りに失敗: {0}")]
    Parse(#[from] lofty::error::FileParseError),
    #[error("タグの書き込みに失敗: {0}")]
    Encode(#[from] lofty::error::FileEncodingError),
    #[error("I/O エラー: {0}")]
    Io(#[from] std::io::Error),
}

/// `file`（読み書きで開いた実体）のタグを `changes` のキーだけ置き換えて保存する。
/// 他のキー・多値は保つ。`pictures` が `Some` なら埋め込み画像を全部捨ててその列に置き換え、
/// `None` なら画像に触らない（D-60）。呼び出し側が tmp にコピーした上で呼ぶこと（対象を直接
/// 書き換えない。SPEC §7.5 tmp + rename）。FLAC / Opus / Vorbis は VorbisComments を直接、
/// 他は lofty の generic `Tag` に Vorbis 名を写像して書く
pub fn write_tag_changes(
    file: &mut File,
    ext: Option<&str>,
    changes: &[TagChange],
    pictures: Option<&[lofty::picture::Picture]>,
) -> Result<(), TagWriteError> {
    let changes: Vec<TagChange> = changes.iter().map(TagChange::normalized).collect();
    file.seek(SeekFrom::Start(0))?;
    let ty = {
        let mut probe = Probe::new(&mut *file);
        if let Some(ft) = ext.and_then(FileType::from_ext) {
            probe = probe.set_file_type(ft);
        }
        let probe = probe.guess_file_type()?;
        probe.file_type().ok_or(TagWriteError::Unrecognized)?
    };
    file.seek(SeekFrom::Start(0))?;
    let opts = ParseOptions::new();
    let write_opts = WriteOptions::default();
    match ty {
        FileType::Flac => {
            let mut f = lofty::flac::FlacFile::read_from(&mut *file, opts)?;
            if f.vorbis_comments().is_none() {
                f.set_vorbis_comments(VorbisComments::default());
            }
            if let Some(vc) = f.vorbis_comments_mut() {
                apply_vorbis(vc, &changes);
            }
            if let Some(pics) = pictures {
                // FLAC の画像は PICTURE ブロック（VorbisComments の METADATA_BLOCK_PICTURE ではない）
                replace_ogg_pictures(&mut f, pics);
            }
            file.seek(SeekFrom::Start(0))?;
            f.save_to(file, write_opts)?;
        }
        FileType::Opus => {
            let mut f = lofty::ogg::OpusFile::read_from(&mut *file, opts)?;
            apply_vorbis(f.vorbis_comments_mut(), &changes);
            if let Some(pics) = pictures {
                replace_ogg_pictures(f.vorbis_comments_mut(), pics);
            }
            file.seek(SeekFrom::Start(0))?;
            f.save_to(file, write_opts)?;
        }
        FileType::Vorbis => {
            let mut f = lofty::ogg::VorbisFile::read_from(&mut *file, opts)?;
            apply_vorbis(f.vorbis_comments_mut(), &changes);
            if let Some(pics) = pictures {
                replace_ogg_pictures(f.vorbis_comments_mut(), pics);
            }
            file.seek(SeekFrom::Start(0))?;
            f.save_to(file, write_opts)?;
        }
        FileType::Mp4
        | FileType::Mpeg
        | FileType::Wav
        | FileType::WavPack
        | FileType::Ape
        | FileType::Aiff => {
            let mut tagged = Probe::new(&mut *file)
                .set_file_type(ty)
                .options(opts)
                .read()?;
            if tagged.primary_tag().is_none() {
                let tt = tagged.primary_tag_type();
                tagged.insert_tag(Tag::new(tt));
            }
            // 読み側は全ブロックを集約する（ID3v2 + ID3v1 など）ので、書きも全ブロックに
            // 同じ変更を当てる。primary だけ書くと副ブロックの旧値が読み戻しに残る
            let types: Vec<TagType> = tagged.tags().iter().map(|t| t.tag_type()).collect();
            for tt in types {
                if let Some(tag) = tagged.tag_mut(tt) {
                    apply_generic(tag, &changes);
                    if let Some(pics) = pictures {
                        // 読み側は最初に画像を持つブロックだけを採る。書きは全ブロックを揃える
                        // （ID3v1 のように画像を持てないブロックは push が無視される）
                        while !tag.pictures().is_empty() {
                            tag.remove_picture(0);
                        }
                        for pic in pics {
                            tag.push_picture(pic.clone());
                        }
                    }
                }
            }
            file.seek(SeekFrom::Start(0))?;
            tagged.save_to(file, write_opts)?;
        }
        other => return Err(TagWriteError::Unsupported(other)),
    }
    Ok(())
}

/// Ogg 系（FLAC の PICTURE ブロック / VorbisComments の METADATA_BLOCK_PICTURE）の画像を
/// `pics` に置き換える。寸法はヘッダから読み（[`crate::media::artwork::sniff`]）、読めない形式は
/// lofty に推定させる（PNG / JPEG 以外は 0×0。プレイヤーは画像本体を見る）。入らない画像は
/// 警告して飛ばす（書き戻し確認で不一致になり op は failed に閉じる）
fn replace_ogg_pictures<S: lofty::ogg::OggPictureStorage>(
    storage: &mut S,
    pics: &[lofty::picture::Picture],
) {
    use lofty::picture::PictureInformation;
    while !storage.pictures().is_empty() {
        storage.remove_picture(0);
    }
    for pic in pics {
        // PNG / JPEG は lofty が色深度まで読む。それ以外（WebP 等）は寸法だけヘッダから補う
        let info = match PictureInformation::from_picture(pic) {
            Ok(i) if i.width > 0 && i.height > 0 => Some(i),
            _ => crate::media::artwork::sniff(pic.data()).map(|i| PictureInformation {
                width: i.width,
                height: i.height,
                color_depth: 0,
                num_colors: 0,
            }),
        };
        if let Err(e) = storage.insert_picture(pic.clone(), info) {
            tracing::warn!(error = %e, "埋め込み画像を書けない");
        }
    }
}

fn apply_vorbis(vc: &mut VorbisComments, changes: &[TagChange]) {
    for c in changes {
        // remove は遅延イテレータなので消費して確定させる
        vc.remove(&c.key).for_each(drop);
        if let Some(values) = &c.values {
            for v in values {
                vc.push(c.key.clone(), v.clone());
            }
        }
    }
}

fn apply_generic(tag: &mut Tag, changes: &[TagChange]) {
    for c in changes {
        // generic Tag は名前付きキーしか持てない。写像できないキーは書けない（読み側も落とす）
        let Some(key) = ItemKey::from_key(TagType::VorbisComments, &c.key) else {
            tracing::warn!(key = %c.key, "この形式には写像できないキーなので書かない");
            continue;
        };
        tag.remove_key(key);
        if let Some(values) = &c.values {
            for v in values {
                tag.push(TagItem::new(key, ItemValue::Text(v.clone())));
            }
        }
    }
}

/// 生成した FLAC（`file` は読み書きで開いた実体。まだ Library に置いていない tmp）に、元ファイル
/// から読んだ [`TransferTags`] を**そのまま**書く（ロスレス正規化。SPEC §7.4）。既存の
/// VorbisComments と画像は置き換える。Vorbis Comment は任意キー・多値を素直に表現できるので
/// 写像は要らない（読み側が既に Vorbis 名へ揃えている）
pub fn write_flac_tags(file: &mut File, tags: &TransferTags) -> Result<(), TagWriteError> {
    use lofty::ogg::OggPictureStorage as _;

    file.seek(SeekFrom::Start(0))?;
    let mut f = lofty::flac::FlacFile::read_from(&mut *file, ParseOptions::new())?;
    let mut vc = VorbisComments::default();
    for (key, value) in &tags.items {
        vc.push(key.clone(), value.clone());
    }
    f.set_vorbis_comments(vc);
    while !f.pictures().is_empty() {
        f.remove_picture(0);
    }
    for pic in &tags.pictures {
        if let Err(e) = f.insert_picture(pic.clone(), None) {
            // 画像が壊れている（寸法を読めない等）ときは音声の正規化を止めない。タグの差分として
            // tag_version が進み、履歴に残る
            tracing::warn!(error = %e, "埋め込み画像を FLAC に写せない");
        }
    }
    file.seek(SeekFrom::Start(0))?;
    f.save_to(file, WriteOptions::default())?;
    Ok(())
}

/// 生成した Opus（`file` は読み書きで開いた tmp）に [`TransferTags`] をそのまま書く（Derived。
/// SPEC §7.6、D-51）。既存の Vorbis Comment と画像は置き換える。画像の寸法は lofty に推定させる
/// （PNG / JPEG 以外は 0×0 になるが、プレイヤーは画像本体を見る）
pub fn write_opus_tags(file: &mut File, tags: &TransferTags) -> Result<(), TagWriteError> {
    use lofty::ogg::OggPictureStorage as _;

    file.seek(SeekFrom::Start(0))?;
    let mut f = lofty::ogg::OpusFile::read_from(&mut *file, ParseOptions::new())?;
    let mut vc = VorbisComments::default();
    for (key, value) in &tags.items {
        vc.push(key.clone(), value.clone());
    }
    for pic in &tags.pictures {
        if let Err(e) = vc.insert_picture(pic.clone(), None) {
            tracing::warn!(error = %e, "画像を Opus に写せない");
        }
    }
    f.set_vorbis_comments(vc);
    file.seek(SeekFrom::Start(0))?;
    f.save_to(file, WriteOptions::default())?;
    Ok(())
}

/// 生成した MP4（`file` は読み書きで開いた tmp）に [`TransferTags`] を書く（aac 系統の Derived。
/// SPEC §7.6、D-75）。既存の ilst は置き換える。Vorbis 名を lofty の `ItemKey` に写像して ilst の
/// 標準 atom（`©ART` `trkn` `disk` `©gen` …）に、写像できないキーは `----:com.apple.iTunes:<KEY>` の
/// フリーフォームに直接置く（lofty の generic `Tag` → `Ilst` は未知キーを捨てるので迂回する）。
/// `iTunNORM` は内部キーの大小文字に関わらず atom 名を固定する。画像は `covr`（JPEG / PNG）
pub fn write_mp4_tags(file: &mut File, tags: &TransferTags) -> Result<(), TagWriteError> {
    use lofty::mp4::{Atom, AtomData, AtomIdent, Ilst};

    use crate::domain::derived::ITUNNORM_KEY;

    file.seek(SeekFrom::Start(0))?;
    let mut f = lofty::mp4::Mp4File::read_from(&mut *file, ParseOptions::new())?;
    let mut generic = Tag::new(TagType::Mp4Ilst);
    let mut freeform: Vec<(String, String)> = Vec::new();
    for (key, value) in &tags.items {
        let mapped = ItemKey::from_key(TagType::VorbisComments, key)
            .filter(|k| k.map_key(TagType::Mp4Ilst).is_some());
        match mapped {
            Some(k) => {
                // 同じ ItemKey に写像されるキーが 2 つある（TRACKTOTAL / TOTALTRACKS）と後勝ち
                generic.push(TagItem::new(k, ItemValue::Text(value.clone())));
            }
            None => {
                let name = if key.eq_ignore_ascii_case(ITUNNORM_KEY) {
                    ITUNNORM_KEY.to_owned()
                } else {
                    key.clone()
                };
                freeform.push((name, value.clone()));
            }
        }
    }
    let mut ilst: Ilst = generic.into();
    for (name, value) in freeform {
        ilst.insert(Atom::new(
            AtomIdent::Freeform {
                mean: "com.apple.iTunes".into(),
                name: name.into(),
            },
            AtomData::UTF8(value),
        ));
    }
    for pic in &tags.pictures {
        ilst.insert_picture(pic.clone());
    }
    f.set_ilst(ilst);
    file.seek(SeekFrom::Start(0))?;
    f.save_to(file, WriteOptions::default())?;
    Ok(())
}
