//! `rgwrite` ジョブ（D-97）。解析値をタグへ書く必要がある行（`tracks.rg_write_due`）を、album ごとに
//! 1 つの編集バッチにする。Library 全体で 1 本（dedup `rgwrite`、payload は空）。
//!
//! - 印は rg の保存（確認が成り立たなくなった行）と album gain の off で立ち、書き込みのバッチを記録した
//!   ときに下りる（[`crate::db::replaygain`]）。巻き戻しでは立たないので、巻き戻した行は書き直さない
//! - 対象は payload に持たず実行時に印から決める。album の移動・missing（戻ったら scan の完了で拾う）・
//!   同じ秒の境界で書き漏れ・書き過ぎが起きない
//! - 開始時に自分の dedup キーを外す（[`crate::db::jobs::release_dedup_key`]）。対象を読んだ後に立った
//!   印は、その書き手が積む新しいジョブが拾う（実行中のジョブへの合流で消えない）
//! - 中身は手動の `POST /api/rg/write` と同じ [`Editor::prepare_rg_write`]（旧値の記録・overlay・
//!   track 単位の tagwrite・巻き戻しはタグ編集と共通。D-48）。ファイルへの反映は tagwrite が行い、
//!   その applied で `rg_written_at` が立ち、Derived が投入される
//! - 同じ album を解析する rg が queued / running の間と、反映待ちの編集がある間はその album を後回しに
//!   し、書ける album を書いた後で**キー付きの後継を [`RETRY_DELAY_SECS`] 後に 1 本だけ積んで終わる**
//!   （album gain が off の album は曲ごとに解析されるので、出揃ってから 1 つのバッチにする。飛ばすと
//!   書き漏れる）。キーを外した自分を `Requeue` すると、rg の完了ごとに積まれるキー付きのジョブと別に
//!   待機ジョブが増え続ける。後継は dedup で常に 1 本
//! - 待つ判定とバッチの記録は別のトランザクションなので、その間に始まった rg が値を変えれば 1 回余分に
//!   書くことがある。値が変わった行は保存で印が立ち直すので次の rgwrite で揃う（最終的な値は収束する）
//! - 冪等: 既に一致している行は op にせず `rg_written_at` だけ立てる。何度走っても結果は同じ

use std::sync::Arc;

use crate::db::jobs as dbjobs;
use crate::db::now_epoch;
use crate::db::replaygain as dbrg;
use crate::edit::{EditError, Editor};
use crate::jobs::{BoxFuture, Handler, HandlerResult, JobContext, JobError, Outcome};

/// 自動書き込みのバッチの説明（履歴に出る）
pub const DESCRIPTION: &str = "ReplayGain の自動書き込み";

/// 後回しにした album があるとき、後継を走らせるまでの秒数（解析・反映待ちの終わりを待つ間の空回りを抑える）
pub const RETRY_DELAY_SECS: i64 = 2;

pub struct RgwriteHandler {
    editor: Arc<Editor>,
    write_tags: bool,
}

impl RgwriteHandler {
    pub fn new(editor: Arc<Editor>, write_tags: bool) -> Self {
        Self { editor, write_tags }
    }
}

impl Handler for RgwriteHandler {
    fn run(&self, ctx: JobContext) -> BoxFuture<'static, HandlerResult> {
        let editor = Arc::clone(&self.editor);
        let write_tags = self.write_tags;
        Box::pin(async move {
            let job_id = ctx.job.id;
            if !write_tags {
                // タグに書かない運用。印は残し、有効にした後の起動時の回収で書く
                tracing::info!(job_id, "[replaygain].write_tags が無効なので書かない");
                return Ok(Outcome::Done);
            }
            ctx.check_cancel().await?;
            // 対象を読む前に dedup キーを外す（読んだ後に立った印は新しいジョブが拾う）
            let groups = ctx
                .db()
                .write(move |c| {
                    let tx = c.unchecked_transaction()?;
                    dbjobs::release_dedup_key(&tx, job_id)?;
                    let mut ready = Vec::new();
                    let mut waiting = 0usize;
                    for g in dbrg::due_groups(&tx)? {
                        if dbrg::analysis_active(&tx, g.album_id, &g.members)? {
                            waiting += 1;
                        } else {
                            ready.push(g);
                        }
                    }
                    tx.commit()?;
                    Ok((ready, waiting))
                })
                .await?;
            let (ready, mut waiting) = groups;
            let mut batches = 0usize;
            for g in ready {
                ctx.check_cancel().await?;
                match editor.prepare_rg_write(Some(DESCRIPTION), g.due).await {
                    Ok(p) => {
                        if p.batch_id.is_some() {
                            batches += 1;
                        }
                    }
                    // 反映待ちの編集が終わってから書く
                    Err(EditError::Pending { .. }) => waiting += 1,
                    Err(e) => return Err(JobError::Failed(anyhow::anyhow!("{e}"))),
                }
            }
            tracing::info!(job_id, batches, waiting, "ReplayGain を自動で書き込んだ");
            if waiting > 0 {
                // 後回しにした album は後継が拾う。既にキー付きのジョブがあればそれに合流する
                let next = ctx
                    .db()
                    .write(move |c| {
                        let now = now_epoch();
                        Ok(dbjobs::enqueue(
                            c,
                            &dbrg::new_write_job().run_after(now + RETRY_DELAY_SECS),
                            now,
                        )?
                        .id())
                    })
                    .await?;
                ctx.jobs().notify_enqueued(&[next]).await;
            }
            Ok(Outcome::Done)
        })
    }
}
