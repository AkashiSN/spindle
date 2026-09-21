// ジョブ画面（SPEC §12.5）の純粋ロジック: 種別ごとの集計、絞り込み、進捗と操作可否の判定

import type { Job, JobState, JobSummary, ListLimits, TypeCounts } from '../api/types'

const TYPE_LABEL: Record<string, string> = {
  scan: 'スキャン',
  rip: 'CD リッピング',
  verify: '遡及照合',
  rg: 'ReplayGain 解析',
  transcode: 'Derived 生成',
  tagwrite: 'タグ書き込み',
  rename: 'リネーム',
  normalize: '正規化',
  thumbnail: 'サムネイル',
  flaccheck: 'FLAC 検査',
  hirescheck: '偽ハイレゾ検出',
  inbox: 'Inbox 取り込み',
  ytdl: 'YouTube ダウンロード',
  playlist_sync: '再生リストの同期',
  gc: 'GC',
  backup: 'バックアップ',
}

export function jobTypeLabel(type: string): string {
  return TYPE_LABEL[type] ?? type
}

export const STATE_LABEL: Record<JobState, string> = {
  queued: '待ち',
  running: '実行中',
  done: '完了',
  failed: '失敗',
  cancelled: '取り消し',
}

export type TypeSummary = {
  type: string
  /** サーバが返した並列度。返さない種別（未知）は null */
  concurrency: number | null
  queued: number
  running: number
  done: number
  failed: number
  cancelled: number
}

/**
 * 種別ごとの待ち行列。件数はサーバの全件集計（`by_type`。一覧は上限付きなので数えない）に並列度を
 * 添える。並列度を持つ種別は 0 件でも出し、種別名順。未知の種別は後ろ
 */
export function summarizeByType(
  byType: Readonly<Record<string, TypeCounts>>,
  concurrency: Readonly<Record<string, number>>,
): TypeSummary[] {
  const map = new Map<string, TypeSummary>()
  const zero = { queued: 0, running: 0, done: 0, failed: 0, cancelled: 0 }
  for (const type of Object.keys(concurrency).sort()) {
    map.set(type, { type, concurrency: concurrency[type] ?? null, ...zero })
  }
  for (const type of Object.keys(byType).sort()) {
    const c = byType[type]
    const row = map.get(type)
    if (row) {
      Object.assign(row, { queued: c.queued, running: c.running, done: c.done, failed: c.failed, cancelled: c.cancelled })
    } else {
      map.set(type, { type, concurrency: null, ...zero, ...c })
    }
  }
  return [...map.values()]
}

export type StateFilter = 'active' | 'done' | 'failed' | 'all'

export function filterJobs(items: readonly Job[], state: StateFilter, type: string | null): Job[] {
  return items.filter((j) => {
    if (type != null && j.type !== type) return false
    switch (state) {
      case 'active':
        return j.state === 'queued' || j.state === 'running'
      case 'done':
        return j.state === 'done'
      case 'failed':
        return j.state === 'failed' || j.state === 'cancelled'
      case 'all':
        return true
    }
  })
}

/** タブに出す件数（サーバの全件集計。一覧は上限付きなので items から数えない） */
export function tabCounts(summary: JobSummary): Record<StateFilter, number> {
  const active = summary.running + summary.queued
  const failed = summary.failed + summary.cancelled
  return { active, done: summary.done, failed, all: active + summary.done + failed }
}

export function jobProgress(j: Job): string {
  if (j.done != null && j.total != null) return `${j.done} / ${j.total}`
  if (j.progress != null) return `${Math.round(j.progress * 100)}%`
  return ''
}

export function canCancel(j: Job): boolean {
  return j.state === 'queued' || j.state === 'running'
}

export function canRetry(j: Job): boolean {
  return j.state === 'failed' || j.state === 'cancelled'
}

/** CPU 系（rg / transcode / flaccheck / hirescheck）の種別か（共通予算の対象。D-73） */
export const CPU_BOUND_TYPES: ReadonlySet<string> = new Set(['rg', 'transcode', 'flaccheck', 'hirescheck'])

/** 種別表の脚注: CPU 系の実行中の合計と予算 */
export function cpuBudgetLabel(rows: readonly TypeSummary[], budget: number | null): string | null {
  if (budget == null) return null
  const running = rows.filter((r) => CPU_BOUND_TYPES.has(r.type)).reduce((n, r) => n + r.running, 0)
  return `CPU 系（rg / transcode / flaccheck / hirescheck）の実行中の合計 ${running} / 予算 ${budget}（= コア数）`
}

/**
 * 表示中のタブが上限で切れているか（切れていれば「表示は最新 N 件まで」）。すべてのタブは 3 つのどれかが
 * 切れていれば合計。`limits` が無い（旧サーバ）なら判定しない
 */
export function shownLimit(state: StateFilter, limits: ListLimits | null, shown: readonly Job[]): number | null {
  if (!limits) return null
  const n = (s: StateFilter) => shown.filter((j) => filterJobs([j], s, null).length > 0).length
  const hit = (s: 'active' | 'done' | 'failed') => n(s) >= limits[s]
  switch (state) {
    case 'active':
    case 'done':
    case 'failed':
      return hit(state) ? limits[state] : null
    case 'all':
      return hit('active') || hit('done') || hit('failed') ? limits.active + limits.done + limits.failed : null
  }
}
