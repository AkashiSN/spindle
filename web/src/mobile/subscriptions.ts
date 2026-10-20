// スマホ版の購読タブの純粋ロジック: 同期の相（ダウンロードが全部終わるまで running）、件数、カードの 1 行、
// 曲ごとの行。「完了」は投入した ytdl がすべて終わった時点（SPEC §12.7「購読」、D-115）

import type { Job, Subscription, SyncResult, UnavailableKind } from '../api/types'
import { noteHeadline } from '../lib/jobs'
import { UNAVAILABLE_LABEL } from '../lib/subscriptions'
import { agoText } from '../lib/syncProgress'
import type { SubscriptionProgress } from './api'

export type SyncPhase = 'idle' | 'running' | 'done' | 'failed'

const live = (j: Job) => j.state === 'queued' || j.state === 'running'
/** 取り消しも「成功していない」ので失敗に数える */
const bad = (j: Job) => j.state === 'failed' || j.state === 'cancelled'

/**
 * sync が null → idle。sync か items のどれかが queued / running → running。
 * すべて終端で、sync が失敗（取り消し含む）か items に失敗がある → failed。それ以外 → done
 */
export function syncPhase(p: SubscriptionProgress): SyncPhase {
  if (p.sync == null) return 'idle'
  if (live(p.sync) || p.items.some(live)) return 'running'
  if (bad(p.sync) || p.items.some(bad)) return 'failed'
  return 'done'
}

/** done は完了した ytdl、failed は失敗・取り消し、total は全部 */
export function downloadCounts(p: SubscriptionProgress): { done: number; total: number; failed: number } {
  let done = 0
  let failed = 0
  for (const j of p.items) {
    if (j.state === 'done') done++
    else if (bad(j)) failed++
  }
  return { done, total: p.items.length, failed }
}

const UNAVAILABLE_ORDER: readonly UnavailableKind[] = ['private', 'deleted', 'unknown']

/** 「非公開 1」「削除 2」（種類ごと。無ければ空） */
function unavailableParts(s: Subscription): string[] {
  const xs = s.last_result?.unavailable ?? []
  return UNAVAILABLE_ORDER.flatMap((k) => {
    const n = xs.filter((u) => u.kind === k).length
    return n > 0 ? [`${UNAVAILABLE_LABEL[k]} ${n}`] : []
  })
}

function firstLine(s: string | null | undefined): string {
  return s?.split('\n', 1)[0]?.trim() ?? ''
}

/** カードの 1 行（`✓ 完了 · 新しく 2 曲を Inbox へ · 非公開 1` など） */
export function cardSummary(s: Subscription, p: SubscriptionProgress): string {
  const sync = p.sync
  if (sync == null) {
    // 同期のジョブが無い（未同期か GC 済み）。前回の結果だけ出す
    const r = s.last_result
    if (r == null) return '未同期'
    if (r.state === 'failed') return `✕ 前回は失敗 · ${r.error ?? '理由不明'}`
    if (r.state === 'cancelled') return '前回は取り消し'
    return r.enqueued.length > 0 ? `前回 ${r.enqueued.length} 曲を投入` : '前回は新しい曲なし'
  }
  const c = downloadCounts(p)
  switch (syncPhase(p)) {
    case 'running':
      if (live(sync) && p.items.length === 0) return sync.state === 'running' ? '再生リストを確認中' : '同期待ち'
      return `ダウンロード ${c.done} / ${c.total} 曲`
    case 'failed':
      if (sync.state === 'cancelled') return '✕ 取り消し'
      if (sync.state === 'failed') return `✕ 失敗 · ${firstLine(sync.last_error) || '同期できなかった'}`
      return `✕ 失敗 · ${c.failed} 曲ダウンロードできなかった`
    default: {
      const parts = ['✓ 完了', c.done > 0 ? `新しく ${c.done} 曲を Inbox へ` : '新しい曲なし', ...unavailableParts(s)]
      return parts.join(' · ')
    }
  }
}

const INBOX_PREFIX = 'Inbox に置いた: '

/**
 * Inbox のパスから動画タイトルを取る。ファイル名は `<YYYYMMDD> <title> [<id>].opus`
 * （downloader::inbox_file_name_for）なので、日付・動画 id・拡張子を外す。形が違えばファイル名のまま
 */
function songName(path: string): string {
  const base = path.split('/').filter(Boolean).pop() ?? path
  const dot = base.lastIndexOf('.')
  const stem = dot > 0 ? base.slice(0, dot) : base
  const title = stem.replace(/^\d{8} /, '').replace(/ \[[A-Za-z0-9_-]+\]$/, '')
  return title === '' ? stem : title
}

function positionText(job: Job): string {
  const v = job.payload?.position
  return typeof v === 'number' ? `#${v}` : '#?'
}

/** 詳細の曲ごとの行 */
export function itemLabel(job: Job): { icon: '✓' | '⟳' | '…' | '✕'; text: string; trail: string } {
  switch (job.state) {
    case 'queued':
      // 失敗の後の再試行待ち（run_after まで）は区別して出す
      return { icon: '…', text: positionText(job), trail: job.attempts > 0 && job.last_error ? '再試行待ち' : '待ち' }
    case 'running':
      return {
        icon: '⟳',
        text: positionText(job),
        trail: job.progress != null ? `${Math.round(job.progress * 100)}%` : 'ダウンロード中',
      }
    case 'failed':
      return { icon: '✕', text: positionText(job), trail: firstLine(job.last_error) || '失敗' }
    case 'cancelled':
      return { icon: '✕', text: positionText(job), trail: '取り消し' }
    case 'done': {
      const head = noteHeadline(job.note)
      if (head.startsWith(INBOX_PREFIX)) return { icon: '✓', text: songName(head.slice(INBOX_PREFIX.length)), trail: 'Inbox へ' }
      if (head.startsWith('取り込み済み')) return { icon: '✓', text: positionText(job), trail: '取り込み済み' }
      if (head.startsWith('プラグインが skip')) return { icon: '✓', text: positionText(job), trail: '飛ばした' }
      return { icon: '✓', text: positionText(job), trail: 'Inbox へ' }
    }
  }
}

/** 最後に同期した時刻の相対表記。1 日を超えたら日で */
export function syncedAgo(at: number | null, now: number): string {
  if (at == null) return '未同期'
  const s = now - at
  if (s >= 86400) return `${Math.floor(s / 86400)} 日前`
  return agoText(s)
}

/** 詳細の内訳の 1 行（`再生リスト 12 曲 · Library 9 · Inbox 1 · 新規 2`） */
export function resultBreakdown(r: SyncResult): string {
  return `再生リスト ${r.entries} 曲 · Library ${r.in_library} · Inbox ${r.in_inbox} · 新規 ${r.enqueued.length}`
}

const UNAVAILABLE_VIDEO: Record<UnavailableKind, string> = {
  private: '非公開の動画',
  deleted: '削除された動画',
  unknown: '取れない動画',
}

/** 取れなかった動画の行（`#31 非公開の動画 · 飛ばした`） */
export function unavailableLine(u: SyncResult['unavailable'][number]): string {
  return `#${u.position} ${UNAVAILABLE_VIDEO[u.kind]} · 飛ばした`
}

/** 曲ごとの行を再生リストの位置順に（位置の無いものは後ろ、同じなら id 順） */
export function sortItems(items: readonly Job[]): Job[] {
  const pos = (j: Job) => {
    const v = j.payload?.position
    return typeof v === 'number' ? v : Number.MAX_SAFE_INTEGER
  }
  return [...items].sort((a, b) => pos(a) - pos(b) || a.id - b.id)
}

/** 同期ボタンの文言と押せるか */
export function syncButton(phase: SyncPhase | null, busy: boolean): { label: string; disabled: boolean } {
  if (busy) return { label: '同期中', disabled: true }
  switch (phase) {
    case 'running':
      return { label: '同期中', disabled: true }
    case 'failed':
      return { label: '再試行', disabled: false }
    default:
      return { label: '同期', disabled: false }
  }
}
