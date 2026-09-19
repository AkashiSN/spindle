// ジョブ画面（SPEC §12.5）の純粋ロジック: 種別ごとの集計、絞り込み、進捗と操作可否の判定

import type { Job, JobState } from '../api/types'

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
  inbox: 'Inbox 取り込み',
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
  failed: number
}

/** 種別ごとの待ち行列。並列度を持つ種別は 0 件でも出し、種別名順。未知の種別は後ろ */
export function summarizeByType(items: readonly Job[], concurrency: Readonly<Record<string, number>>): TypeSummary[] {
  const map = new Map<string, TypeSummary>()
  for (const type of Object.keys(concurrency).sort()) {
    map.set(type, { type, concurrency: concurrency[type] ?? null, queued: 0, running: 0, failed: 0 })
  }
  for (const j of items) {
    let row = map.get(j.type)
    if (!row) {
      row = { type: j.type, concurrency: null, queued: 0, running: 0, failed: 0 }
      map.set(j.type, row)
    }
    if (j.state === 'queued') row.queued++
    else if (j.state === 'running') row.running++
    else if (j.state === 'failed') row.failed++
  }
  return [...map.values()]
}

export type StateFilter = 'active' | 'failed' | 'all'

export function filterJobs(items: readonly Job[], state: StateFilter, type: string | null): Job[] {
  return items.filter((j) => {
    if (type != null && j.type !== type) return false
    switch (state) {
      case 'active':
        return j.state === 'queued' || j.state === 'running'
      case 'failed':
        return j.state === 'failed' || j.state === 'cancelled'
      case 'all':
        return true
    }
  })
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
