//! アートワーク（SPEC §5 / §7.1、P1-3、D-49）。
//!
//! - album のアートワークは **ディレクトリの同梱カバー画像**（[`cover_rank`] の名前）があれば
//!   それ、無ければ**最初のトラックの埋め込み画像**（[`pick_embedded`]。front cover を優先）
//! - 画像はバイト列の SHA-256 でハッシュアドレスし、原画像を元の形式のまま
//!   `thumbs/<hex>/orig.<ext>` に置く。サムネイルは常に WebP（`thumbs/<hex>/<size>.webp`。
//!   `thumbnail` ジョブが ffmpeg で作る）。Library には何も書かない
//! - 形式の判別はバイト列のヘッダ（[`sniff`]）。拡張子やタグの MIME は信用しない

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use lofty::picture::{Picture, PictureType};
use sha2::{Digest, Sha256};

/// サムネイルの一辺（長辺をこの長さに縮める。小さい画像は拡大しない）
pub const THUMB_SIZES: [u32; 2] = [256, 768];

/// 受け入れる画像の上限（D-49 の同梱カバー画像、D-60 のアップロード）。これより大きいファイルは
/// 画像とみなさない。退避した埋め込み画像（[`ArtworkStore::read_original`]）には適用しない
pub const MAX_COVER_BYTES: u64 = 32 * 1024 * 1024;

/// 同梱カバー画像として認識する基本名（優先順）
const COVER_NAMES: [&str; 3] = ["cover", "folder", "front"];
/// 同梱カバー画像として認識する拡張子（優先順）
const COVER_EXTS: [&str; 4] = ["jpg", "jpeg", "png", "webp"];

/// ファイル名が同梱カバー画像なら優先順位（小さいほど優先）。大文字小文字は区別しない
/// （ZFS insensitive と同じ）。名前の優先が拡張子の優先より強い
pub fn cover_rank(name: &str) -> Option<u8> {
    let (stem, ext) = name.rsplit_once('.')?;
    let stem = stem.to_ascii_lowercase();
    let ext = ext.to_ascii_lowercase();
    let n = COVER_NAMES.iter().position(|c| *c == stem)?;
    let e = COVER_EXTS.iter().position(|c| *c == ext)?;
    Some((n * COVER_EXTS.len() + e) as u8)
}

/// 画像ヘッダから読んだ種別と寸法
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageInfo {
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

/// 一辺の上限。これを超える寸法はヘッダの誤読とみなす（一般的なデコーダの上限に合わせる）
pub const MAX_DIMENSION: u32 = 16_384;

/// バイト列のヘッダから画像の種別と寸法を読む。magic だけ一致して寸法が読めない・0・上限超の
/// 入力は画像とみなさない（壊れた同梱画像を album に確定させない）
pub fn sniff(bytes: &[u8]) -> Option<ImageInfo> {
    use imagesize::ImageType;
    let mime = match imagesize::image_type(bytes).ok()? {
        ImageType::Jpeg => "image/jpeg",
        ImageType::Png => "image/png",
        ImageType::Webp => "image/webp",
        ImageType::Gif => "image/gif",
        ImageType::Bmp => "image/bmp",
        ImageType::Tiff => "image/tiff",
        _ => return None,
    };
    let size = imagesize::blob_size(bytes).ok()?;
    let width = u32::try_from(size.width).ok()?;
    let height = u32::try_from(size.height).ok()?;
    if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
        return None;
    }
    Some(ImageInfo {
        mime,
        width,
        height,
    })
}

/// MIME に対応する拡張子（原画像の保存名に使う）
pub fn ext_of_mime(mime: &str) -> &'static str {
    match mime {
        "image/jpeg" => "jpg",
        "image/png" => "png",
        "image/webp" => "webp",
        "image/gif" => "gif",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        _ => "bin",
    }
}

/// 埋め込み画像から album のアートワークに使う 1 枚を選ぶ: front cover があればそれ、
/// 無ければ最初の 1 枚（lofty が並べた順 = ファイル内の順）
pub fn pick_embedded(pictures: &[Picture]) -> Option<&Picture> {
    pictures
        .iter()
        .find(|p| p.pic_type() == PictureType::CoverFront)
        .or_else(|| pictures.first())
}

/// トラック自身の埋め込み画像（キャッシュへ置いた後。`tracks.artwork_id` の元。D-61）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackPicture {
    pub sha256: [u8; 32],
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
    /// サムネイルがまだ無い（呼び出し側が thumbnail ジョブを投入する）
    pub needs_thumbs: bool,
}

/// トラックの埋め込み画像から 1 枚（[`pick_embedded`]）を選び、ヘッダで判別してキャッシュへ置く。
/// 画像が無い・認識できないときは `Ok(None)`。キャッシュへ置けない（I/O 失敗）ときは `Err`（呼び出し側が
/// 「画像なし」と区別する: 既存の `artwork_id` を消してはいけない。D-61）。
/// `verify` が偽なら、既に同じ長さの原画像があれば stat だけで済ませる（同じ画像を持つ数千トラックで
/// 実体を読み直さない）。真なら [`ArtworkStore::put_original`] が内容のハッシュを照合して、同じ長さの
/// 破損も置き直す（deep scan）
pub fn register_track_picture(
    store: &ArtworkStore,
    pictures: &[Picture],
    verify: bool,
) -> std::io::Result<Option<TrackPicture>> {
    let Some(pic) = pick_embedded(pictures) else {
        return Ok(None);
    };
    let Some(info) = sniff(pic.data()) else {
        return Ok(None);
    };
    let hash = ArtworkStore::hash_of(pic.data());
    if verify || !store.has_original_of_len(&hash, info.mime, pic.data().len() as u64) {
        store.put_original(&hash, info.mime, pic.data())?;
    }
    Ok(Some(TrackPicture {
        sha256: hash,
        mime: info.mime,
        width: info.width,
        height: info.height,
        bytes: pic.data().len(),
        needs_thumbs: !store.missing_thumbs(&hash).is_empty(),
    }))
}

// ---------------------------------------------------------------- キャッシュ

/// ハッシュアドレスのアートワーク置き場（`<data>/thumbs`）
#[derive(Debug, Clone)]
pub struct ArtworkStore {
    dir: PathBuf,
}

impl ArtworkStore {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn hash_of(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    pub fn hex(hash: &[u8]) -> String {
        hash.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn entry_dir(&self, hash: &[u8]) -> PathBuf {
        self.dir.join(Self::hex(hash))
    }

    pub fn original_path(&self, hash: &[u8], mime: &str) -> PathBuf {
        self.entry_dir(hash)
            .join(format!("orig.{}", ext_of_mime(mime)))
    }

    pub fn thumb_path(&self, hash: &[u8], size: u32) -> PathBuf {
        self.entry_dir(hash).join(format!("{size}.webp"))
    }

    pub fn has_original(&self, hash: &[u8], mime: &str) -> bool {
        self.original_path(hash, mime).is_file()
    }

    /// 原画像が期待どおりの長さで存在するか（欠損・長さの違う破損の検出。同じ長さの破損は
    /// [`Self::put_original`] のハッシュ照合で置き直す）
    pub fn has_original_of_len(&self, hash: &[u8], mime: &str, len: u64) -> bool {
        std::fs::metadata(self.original_path(hash, mime))
            .is_ok_and(|m| m.is_file() && m.len() == len)
    }

    /// まだ無いサムネイルの一辺
    pub fn missing_thumbs(&self, hash: &[u8]) -> Vec<u32> {
        THUMB_SIZES
            .into_iter()
            .filter(|s| !self.thumb_path(hash, *s).is_file())
            .collect()
    }

    /// 原画像を読む（サイズ上限なし。退避した埋め込み画像は [`MAX_COVER_BYTES`] を超えることが
    /// あり、巻き戻しには全体が要る）。内容の SHA-256 が `hash` と違えば `None`（壊れている）。
    /// 無ければ `None`
    pub fn read_original(&self, hash: &[u8], mime: &str) -> std::io::Result<Option<Vec<u8>>> {
        let path = self.original_path(hash, mime);
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if Self::hash_of(&bytes) != hash {
            tracing::warn!(path = %path.display(), "原画像の内容がハッシュと一致しない");
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    /// `thumbs/<hex>/` の mtime を今にする。GC 区分 E の猶予（dir の mtime から 24 時間）を
    /// 延ばすために、既にある画像を置き直す側（アップロード）が呼ぶ。dir が無ければ何もしない
    pub fn touch(&self, hash: &[u8]) -> std::io::Result<()> {
        match File::open(self.entry_dir(hash)) {
            Ok(dir) => dir.set_modified(std::time::SystemTime::now()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// 原画像を置く（tmp + rename）。既にあるファイルは**内容の SHA-256 が一致するときだけ**
    /// 流用し、そのときも dir の mtime は今にする（GC 区分 E の猶予を置き直しの時点から数え直す）。
    /// （ハッシュアドレスの不変条件。長さが同じでも壊れていれば置き直す）。返り値は置いた先
    pub fn put_original(&self, hash: &[u8], mime: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
        let path = self.original_path(hash, mime);
        if let Ok(existing) = std::fs::read(&path) {
            if Self::hash_of(&existing) == hash {
                self.touch(hash)?;
                return Ok(path);
            }
            tracing::warn!(path = %path.display(), "原画像の内容がハッシュと一致しないので置き直す");
        }
        let dir = self.entry_dir(hash);
        std::fs::create_dir_all(&dir)?;
        // 同じハッシュ = 同じ内容なので、並行して置かれても rename の上書きで壊れない
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp = dir.join(format!(".orig-{}-{nonce}.tmp", std::process::id()));
        let result = (|| -> std::io::Result<()> {
            let mut f = File::create(&tmp)?;
            f.write_all(bytes)?;
            f.sync_all()?;
            std::fs::rename(&tmp, &path)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        result.map(|()| path)
    }
}
