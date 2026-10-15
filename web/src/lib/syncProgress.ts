// Mac の同期の進捗（端末タブ）の書式と判定。表示の経過時間はサーバが計算した age を受信時刻に直して持つ

import type { SyncProgress } from '../api/types'
import { formatBytes } from './settings'

/** これを超えて進捗が来なければ「応答なし」と見なす（秒） */
export const STALE_SECS = 30

/** 進み具合 0..1。バイトの総量があればバイト、無ければ件数 */
export function progressRatio(p: SyncProgress): number {
  const r = p.bytes_total > 0 ? p.bytes_done / p.bytes_total : p.total > 0 ? p.done / p.total : 0
  return Math.min(1, Math.max(0, r))
}

/** 「45 秒前」「1 分前」「1 時間前」 */
export function agoText(secs: number): string {
  const s = Math.max(0, Math.floor(secs))
  if (s < 60) return `${s} 秒前`
  if (s < 3600) return `${Math.floor(s / 60)} 分前`
  return `${Math.floor(s / 3600)} 時間前`
}

function remaining(secs: number): string {
  const s = Math.max(0, Math.round(secs))
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`
}

/** 1 行の要約。`ageSecs` は最後の進捗からの経過秒 */
export function progressSummary(p: SyncProgress, ageSecs: number): string {
  const parts = [`${p.done} / ${p.total} 件`]
  const hasBytes = p.bytes_total > 0
  if (hasBytes) parts.push(`${formatBytes(p.bytes_done)} / ${formatBytes(p.bytes_total)}`)
  if (ageSecs > STALE_SECS) {
    parts.push(`応答なし（最後の進捗: ${agoText(ageSecs)}）`)
  } else if (hasBytes && p.rate > 0) {
    parts.push(`${formatBytes(p.rate)}/s`)
    parts.push(`残り ${remaining((p.bytes_total - p.bytes_done) / p.rate)}`)
  }
  return parts.join(' · ')
}

export function phaseLabel(phase: SyncProgress['phase']): string {
  switch (phase) {
    case 'delete':
      return '削除中'
    case 'move':
      return '移動中'
    case 'fetch':
      return '受信中'
    case 'add':
      return '追加中'
    case 'playlist':
      return 'プレイリスト'
  }
}

/** 受信時刻（ms）。サーバの age から逆算する */
export function receivedAtOf(p: Pick<SyncProgress, 'age'>, nowMs: number): number {
  return nowMs - p.age * 1000
}
