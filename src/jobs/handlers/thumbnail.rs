//! `thumbnail` ジョブ（SPEC §8、P1-3、D-49）。payload は `{"artwork_id"}`、dedup key は
//! `thumbnail:<artwork_id>`。
//!
//! `thumbs/<hex>/orig.<ext>`（スキャンが置いた原画像）から、まだ無い一辺
//! （[`THUMB_SIZES`]）の WebP を ffmpeg で作る。長辺をその長さに縮め、小さい画像は拡大しない。
//! tmp に書いて rename するので、途中で落ちても壊れたサムネイルは残らない。何度実行しても
//! 結果は同じ（既にあるサイズは飛ばす）

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::db::artwork as dbart;
use crate::jobs::process::{ExternalCommand, PathStyle};
use crate::jobs::{
    BoxFuture, Handler, HandlerResult, JobContext, JobError, JobType, NewJob, Outcome,
};
use crate::media::artwork::{ArtworkStore, ThumbFormat, THUMB_SIZES};

/// 1 枚の変換の上限
const STEP_TIMEOUT: Duration = Duration::from_secs(60);
/// WebP の品質（0..100）
const WEBP_QUALITY: &str = "82";
/// JPEG の品質（mjpeg の -q:v。2 ≈ 90%）
const JPEG_QUALITY: &str = "2";

pub fn dedup_key(artwork_id: i64) -> String {
    format!("thumbnail:{artwork_id}")
}

pub fn new_thumbnail_job(artwork_id: i64) -> NewJob {
    NewJob::new(
        JobType::Thumbnail,
        serde_json::json!({ "artwork_id": artwork_id }),
    )
    .dedup_key(dedup_key(artwork_id))
}

pub struct ThumbnailHandler {
    store: Arc<ArtworkStore>,
    ffmpeg: std::path::PathBuf,
}

impl ThumbnailHandler {
    pub fn new(store: Arc<ArtworkStore>, ffmpeg: impl AsRef<Path>) -> Self {
        Self {
            store,
            ffmpeg: ffmpeg.as_ref().to_path_buf(),
        }
    }
}

/// `src` から一辺 `size` の WebP / JPEG を `dst` に作る（tmp + rename。失敗したら tmp を消す）
pub async fn make_thumb(
    ffmpeg: &Path,
    src: &Path,
    dst: &Path,
    size: u32,
    format: ThumbFormat,
    job_id: i64,
    token: &tokio_util::sync::CancellationToken,
) -> Result<(), JobError> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dst.with_extension(format!("{}.tmp-{job_id}-{nonce}", format.ext()));
    // 長辺を size に。小さい画像は拡大しない（min）。偶数丸めは不要（WebP に制約なし、mjpeg も奇数で書ける）
    let scale =
        format!("scale='min({size},iw)':'min({size},ih)':force_original_aspect_ratio=decrease");
    let codec: &[&str] = match format {
        ThumbFormat::WebP => &["-c:v", "libwebp", "-quality", WEBP_QUALITY, "-f", "webp"],
        // mjpeg はフルレンジの yuvj420p。-f image2 で 1 枚の JPEG
        ThumbFormat::Jpeg => &[
            "-c:v",
            "mjpeg",
            "-pix_fmt",
            "yuvj420p",
            "-q:v",
            JPEG_QUALITY,
            "-f",
            "image2",
        ],
    };
    let result = ExternalCommand::new(ffmpeg)
        .path_style(PathStyle::DotSlash)
        .args(["-hide_banner", "-nostdin", "-loglevel", "error", "-y", "-i"])
        .path_arg(src)
        .args(["-frames:v", "1", "-vf", &scale])
        .args(codec)
        .path_arg(&tmp)
        .timeout(STEP_TIMEOUT)
        .run(token)
        .await;
    let placed = match result {
        Ok(_) => tokio::fs::rename(&tmp, dst).await.map_err(|e| {
            JobError::Failed(anyhow::anyhow!(
                "サムネイルを置けない {}: {e}",
                dst.display()
            ))
        }),
        Err(e) => Err(e.into()),
    };
    if placed.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    placed
}

impl Handler for ThumbnailHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let store = Arc::clone(&self.store);
        let ffmpeg = self.ffmpeg.clone();
        Box::pin(async move {
            let job_id = ctx.job.id;
            let Some(artwork_id) = ctx.job.payload.get("artwork_id").and_then(|v| v.as_i64())
            else {
                return Err(JobError::Failed(anyhow::anyhow!(
                    "payload に整数の artwork_id が無い: {}",
                    ctx.job.payload
                )));
            };
            let Some(art) = ctx.db().read(move |c| dbart::get(c, artwork_id)).await? else {
                tracing::info!(job_id, artwork_id, "artwork が無いので何もしない");
                return Ok(Outcome::Done);
            };
            let src = store.original_path(&art.sha256, &art.mime);
            if !src.is_file() {
                // 参照している album の再解決を予約する（スキャンの Phase 5 も原画像の欠損を見るが、
                // 予約しておけば同梱画像 / トラックが変わらなくても確実に置き直される）
                let n = ctx
                    .db()
                    .write(move |c| dbart::mark_unresolved_by_artwork(c, artwork_id))
                    .await?;
                return Err(JobError::Failed(anyhow::anyhow!(
                    "原画像が無い: {}（次のスキャンで置き直される。album {n} 件を予約）",
                    src.display()
                )));
            }
            let missing = store.missing_thumbs(&art.sha256);
            let total = THUMB_SIZES.len() as i64;
            ctx.progress(total - missing.len() as i64, total).await?;
            let token = ctx.cancel_token();
            for (i, size) in missing.iter().enumerate() {
                ctx.check_cancel().await?;
                let dst = store.thumb_path(&art.sha256, *size);
                make_thumb(
                    &ffmpeg,
                    &src,
                    &dst,
                    *size,
                    ThumbFormat::WebP,
                    job_id,
                    &token,
                )
                .await?;
                ctx.progress(total - missing.len() as i64 + i as i64 + 1, total)
                    .await?;
            }
            tracing::debug!(
                job_id,
                artwork_id,
                made = missing.len(),
                "サムネイル生成完了"
            );
            Ok(Outcome::Done)
        })
    }
}
