// 購読の節（SPEC §12.6、D-78、P4-16）の純粋ロジック: URL の検査、結果の 1 行化と詳細の行、同期ジョブの状態

import type { Job, Subscription, SyncBlocked, SyncResult, UnavailableKind } from '../api/types'

/** YouTube の再生リスト URL から `list=` を取る。YouTube 以外・`list=` 無しは null（サーバと同じ規則） */
export function listIdFromUrl(url: string): string | null {
  let u: URL
  try {
    u = new URL(url.trim())
  } catch {
    return null
  }
  if (u.protocol !== 'http:' && u.protocol !== 'https:') return null
  const host = u.hostname.toLowerCase()
  if (!['youtube.com', 'youtu.be'].some((d) => host === d || host.endsWith(`.${d}`))) return null
  const id = u.searchParams.get('list')?.trim() ?? ''
  return /^[A-Za-z0-9_-]+$/.test(id) ? id : null
}

/** 購読ごとの同期ジョブ（queued / running のもの）。dedup は `playlist_sync:<id>` */
export function activeSyncJob(jobs: readonly Job[], subscriptionId: number): Job | undefined {
  return jobs.find(
    (j) => j.type === 'playlist_sync' && (j.state === 'queued' || j.state === 'running') && j.subject === `subscription #${subscriptionId}`,
  )
}

export const UNAVAILABLE_LABEL: Record<UnavailableKind, string> = {
  private: '非公開',
  deleted: '削除',
  unknown: '取れない',
}

/** 結果の 1 行（一覧の「結果」列）。走行中なら状態、無ければ「未同期」 */
export function subscriptionStatusLabel(s: Subscription, job: Job | undefined): string {
  if (job != null) {
    if (job.state === 'running') return '同期中'
    return job.attempts > 0 && job.last_error ? `再試行待ち（${job.last_error}）` : '同期待ち'
  }
  const r = s.last_result
  if (r == null) return s.sync_requested_at != null ? '同期待ち' : '未同期'
  if (r.state === 'failed') return `失敗: ${r.error ?? '理由不明'}`
  if (r.state === 'cancelled') return '取り消し'
  return syncSummary(r)
}

/** 完了した同期の要約（サーバの note と同じ内容） */
export function syncSummary(r: SyncResult): string {
  const parts = [`${r.entries} 件中 Library に ${r.in_library}`]
  if (r.enqueued.length > 0) parts.push(`${r.enqueued.length} 件を投入`)
  if (r.in_inbox > 0) parts.push(`${r.in_inbox} 件は Inbox で取り込み中`)
  if (r.running.length > 0) parts.push(`${r.running.length} 件は別の投入が走行中`)
  if (r.deferred > 0) parts.push(`${r.deferred} 件は次回`)
  if (r.unavailable.length > 0) parts.push(`${r.unavailable.length} 件は取れない`)
  if (r.elsewhere.length > 0) parts.push(`${r.elsewhere.length} 件は別の album`)
  const a = r.align
  if (a != null) {
    if (a.moved > 0 || a.renamed > 0) parts.push(`番号を ${a.moved} 件揃え ${a.renamed} 件を改名`)
    if (a.blocked.length > 0) parts.push(`${a.blocked.length} 件は揃えられない`)
    if ((a.rename_conflicts?.length ?? 0) > 0) parts.push(`${a.rename_conflicts!.length} 件は改名できない`)
  }
  return parts.join('、')
}

export function blockedReasonLabel(b: SyncBlocked): string {
  switch (b.reason.kind) {
    case 'number_taken':
      return `${b.position} 番は track #${b.reason.by_track_id} が使っている（SOURCE_URL 無しか再生リスト外）`
    case 'duplicate_source_url':
      return '同じ SOURCE_URL の行が複数ある'
    case 'other_disc':
      return `disc ${b.reason.disc_no} の行`
    case 'duplicate_entry':
      return `同じ動画が再生リストに複数回ある（${b.reason.positions.map((p) => `#${p}`).join('、')}）`
  }
}

/** 詳細の行（一覧の行を開いたとき）。空なら詳細なし */
export function syncDetailLines(r: SyncResult): string[] {
  const out: string[] = []
  if (r.unavailable.length > 0) {
    out.push(
      `取れない: ${r.unavailable.map((u) => `#${u.position} ${u.id}（${UNAVAILABLE_LABEL[u.kind]}）`).join('、')}`,
    )
  }
  if (r.elsewhere.length > 0) {
    out.push(`別の album にある: ${r.elsewhere.map((e) => `#${e.position} → ${e.rel_path}`).join('、')}`)
  }
  if (r.running.length > 0) out.push(`別の投入が走行中: ${r.running.map((p) => `#${p}`).join('、')}`)
  if (r.deferred > 0) out.push(`上限で次回に持ち越し: ${r.deferred} 件`)
  const a = r.align
  if (a != null) {
    for (const b of a.blocked) out.push(`揃えられない: #${b.position}（今 ${b.current_no ?? '無番'}）: ${blockedReasonLabel(b)}`)
    if (a.tags != null) {
      out.push(`番号のバッチ #${a.tags.batch_id}: 適用 ${a.tags.applied} / 衝突 ${a.tags.conflict} / 失敗 ${a.tags.failed}`)
    }
    if (a.rename != null) {
      out.push(`改名のバッチ #${a.rename.batch_id}: 適用 ${a.rename.applied} / 衝突 ${a.rename.conflict} / 失敗 ${a.rename.failed}`)
    }
    for (const c of a.rename_conflicts ?? []) out.push(`改名できない: track #${c.track_id}: ${c.reason}`)
    if (a.outsiders > 0) out.push(`再生リストに無い SOURCE_URL 付きの行: ${a.outsiders} 件（触らない）`)
    if (a.unnumbered > 0) out.push(`SOURCE_URL の無い行: ${a.unnumbered} 件（触らない）`)
  }
  return out
}
