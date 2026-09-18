//! アートワーク（SPEC §5 / §7.1、docs/TASKS.md P1-3、D-49）: 同梱カバー画像の名前、画像の判別、
//! 埋め込み画像の選択、ハッシュアドレスのキャッシュ

use spindle::media::artwork::{
    cover_rank, ext_of_mime, pick_embedded, sniff, ArtworkStore, ImageInfo, MAX_COVER_BYTES,
    THUMB_SIZES,
};

/// 1x1 の JPEG（最小のヘッダ + SOF0）
fn tiny_jpeg() -> Vec<u8> {
    let mut v = vec![0xFF, 0xD8];
    // SOF0: 長さ 11、精度 8、高さ 1、幅 1、1 成分
    v.extend_from_slice(&[
        0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01, 0x01, 0x01, 0x11, 0x00,
    ]);
    v.extend_from_slice(&[0xFF, 0xD9]);
    v
}

/// 2x3 の PNG（シグネチャ + IHDR）
fn tiny_png() -> Vec<u8> {
    let mut v = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
    v.extend_from_slice(&[0x00, 0x00, 0x00, 0x0D]);
    v.extend_from_slice(b"IHDR");
    v.extend_from_slice(&[0, 0, 0, 2, 0, 0, 0, 3, 8, 2, 0, 0, 0]);
    v.extend_from_slice(&[0, 0, 0, 0]);
    v
}

#[test]
fn cover_file_names_are_ranked_case_insensitively() {
    assert_eq!(cover_rank("cover.jpg"), Some(0));
    assert_eq!(cover_rank("Cover.JPG"), Some(0));
    assert!(cover_rank("cover.jpeg").unwrap() > cover_rank("cover.jpg").unwrap());
    assert!(cover_rank("cover.png").unwrap() > cover_rank("cover.jpeg").unwrap());
    assert!(cover_rank("cover.webp").unwrap() > cover_rank("cover.png").unwrap());
    // 名前の優先 > 拡張子の優先
    assert!(cover_rank("folder.jpg").unwrap() > cover_rank("cover.webp").unwrap());
    assert!(cover_rank("front.jpg").unwrap() > cover_rank("folder.webp").unwrap());
    assert_eq!(cover_rank("back.jpg"), None);
    assert_eq!(cover_rank("cover.gif"), None);
    assert_eq!(cover_rank("cover"), None);
    assert_eq!(cover_rank("01 cover.jpg"), None);
}

#[test]
fn sniff_detects_type_and_dimensions_from_bytes() {
    let j = sniff(&tiny_jpeg()).unwrap();
    assert_eq!(
        j,
        ImageInfo {
            mime: "image/jpeg",
            width: 1,
            height: 1,
        }
    );
    let p = sniff(&tiny_png()).unwrap();
    assert_eq!(p.mime, "image/png");
    assert_eq!((p.width, p.height), (2, 3));
    assert_eq!(sniff(b"not an image at all"), None);
    assert_eq!(sniff(b""), None);
}

#[test]
fn sniff_rejects_magic_without_readable_or_sane_dimensions() {
    // magic だけ（SOF が無い）
    assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]), None);
    // PNG のシグネチャだけ
    assert_eq!(
        sniff(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
        None
    );
    // 寸法 0
    let mut zero = tiny_png();
    zero[16..20].copy_from_slice(&[0, 0, 0, 0]);
    assert_eq!(sniff(&zero), None);
    // 上限超（幅 65535）
    let mut huge = tiny_png();
    huge[16..20].copy_from_slice(&[0, 0, 0xFF, 0xFF]);
    assert_eq!(sniff(&huge), None);
}

#[test]
fn extension_follows_mime() {
    assert_eq!(ext_of_mime("image/jpeg"), "jpg");
    assert_eq!(ext_of_mime("image/png"), "png");
    assert_eq!(ext_of_mime("image/webp"), "webp");
    assert_eq!(ext_of_mime("image/gif"), "gif");
    assert_eq!(ext_of_mime("application/octet-stream"), "bin");
}

#[test]
fn embedded_picture_prefers_front_cover_then_first() {
    use lofty::picture::{MimeType, Picture, PictureType};
    let pic = |ty: PictureType, data: &[u8]| {
        Picture::unchecked(data.to_vec())
            .pic_type(ty)
            .mime_type(MimeType::Jpeg)
            .build()
    };
    let pics = vec![
        pic(PictureType::CoverBack, b"back"),
        pic(PictureType::CoverFront, b"front"),
        pic(PictureType::Other, b"other"),
    ];
    assert_eq!(pick_embedded(&pics).unwrap().data(), b"front");
    let pics = vec![
        pic(PictureType::CoverBack, b"back"),
        pic(PictureType::Other, b"other"),
    ];
    assert_eq!(pick_embedded(&pics).unwrap().data(), b"back");
    assert!(pick_embedded(&[]).is_none());
}

#[test]
fn store_paths_are_hash_addressed_and_put_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtworkStore::new(dir.path().join("thumbs"));
    let bytes = tiny_jpeg();
    let hash = ArtworkStore::hash_of(&bytes);
    assert_eq!(hash.len(), 32);
    let hex = ArtworkStore::hex(&hash);
    assert_eq!(hex.len(), 64);

    let p = store.put_original(&hash, "image/jpeg", &bytes).unwrap();
    assert_eq!(p, dir.path().join("thumbs").join(&hex).join("orig.jpg"));
    assert_eq!(std::fs::read(&p).unwrap(), bytes);
    // 2 回目は上書きしない（同じ内容なので何もしない）
    let before = std::fs::metadata(&p).unwrap().modified().unwrap();
    let p2 = store.put_original(&hash, "image/jpeg", &bytes).unwrap();
    assert_eq!(p, p2);
    assert_eq!(std::fs::metadata(&p).unwrap().modified().unwrap(), before);
    // 長さが違う（壊れている）なら置き直す
    std::fs::write(&p, b"broken").unwrap();
    store.put_original(&hash, "image/jpeg", &bytes).unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), bytes);
    // 同じ長さでも内容のハッシュが違えば置き直す（ハッシュアドレスの不変条件）
    let mut same_len = bytes.clone();
    same_len[3] ^= 0x01;
    std::fs::write(&p, &same_len).unwrap();
    store.put_original(&hash, "image/jpeg", &bytes).unwrap();
    assert_eq!(std::fs::read(&p).unwrap(), bytes);
    assert!(store.has_original_of_len(&hash, "image/jpeg", bytes.len() as u64));
    assert!(!store.has_original_of_len(&hash, "image/jpeg", 1));
    assert_eq!(store.original_path(&hash, "image/jpeg"), p);
    assert!(store.has_original(&hash, "image/jpeg"));

    for size in THUMB_SIZES {
        let t = store.thumb_path(&hash, size);
        assert_eq!(
            t,
            dir.path()
                .join("thumbs")
                .join(&hex)
                .join(format!("{size}.webp"))
        );
        assert!(!t.exists());
    }
    assert_eq!(store.missing_thumbs(&hash), THUMB_SIZES.to_vec());
    // 一時ファイルは残らない
    let entries: Vec<_> = std::fs::read_dir(dir.path().join("thumbs").join(&hex))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(entries, vec!["orig.jpg".to_owned()]);
}

/// 退避した旧画像はアップロードの上限（`MAX_COVER_BYTES`）に縛られない。巻き戻しには全体が要るので
/// 読み戻しも上限なし（D-60。codex の指摘: 切り詰めるとハッシュが合わず巻き戻しが必ず失敗する）
#[test]
fn read_original_returns_the_whole_image_even_beyond_the_upload_limit() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtworkStore::new(dir.path().join("thumbs"));
    let mut big = tiny_jpeg();
    big.resize(MAX_COVER_BYTES as usize + 4096, 0x5a);
    let hash = ArtworkStore::hash_of(&big);
    store.put_original(&hash, "image/jpeg", &big).unwrap();
    let got = store.read_original(&hash, "image/jpeg").unwrap().unwrap();
    assert_eq!(got.len(), big.len());
    assert_eq!(ArtworkStore::hash_of(&got), hash);
    // 無ければ None、壊れていれば None
    assert!(store
        .read_original(&[9u8; 32], "image/jpeg")
        .unwrap()
        .is_none());
    std::fs::write(store.original_path(&hash, "image/jpeg"), b"broken").unwrap();
    assert!(store.read_original(&hash, "image/jpeg").unwrap().is_none());
}

/// 既にある画像を置き直したら dir の mtime は今になる（GC 区分 E の 24 時間の猶予を、アップロード
/// し直した時点から数え直すため。codex の指摘: 早期 return だけだと古い未参照 dir が embed 前に消える）
#[test]
fn putting_an_existing_original_again_refreshes_the_entry_dir_mtime() {
    let dir = tempfile::tempdir().unwrap();
    let store = ArtworkStore::new(dir.path().join("thumbs"));
    let bytes = tiny_jpeg();
    let hash = ArtworkStore::hash_of(&bytes);
    store.put_original(&hash, "image/jpeg", &bytes).unwrap();
    let entry = store.dir().join(ArtworkStore::hex(&hash));
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(5 * 24 * 3600);
    std::fs::File::open(&entry)
        .unwrap()
        .set_modified(old)
        .unwrap();
    store.put_original(&hash, "image/jpeg", &bytes).unwrap();
    let mtime = std::fs::metadata(&entry).unwrap().modified().unwrap();
    assert!(
        mtime.duration_since(old).unwrap() > std::time::Duration::from_secs(4 * 24 * 3600),
        "dir の mtime が更新されていない"
    );
    // 無い hash の touch は何もしない
    store.touch(&[7u8; 32]).unwrap();
}
