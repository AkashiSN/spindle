// 編集履歴画面（SPEC §12.4）の表示ロジック。React に依存しない部分

import type { HistoryItem } from '../api/types'

/** 行の説明。無ければ「(#N の巻き戻し)」、それも無ければ種別 */
export function batchLabel(b: HistoryItem): string {
  if (b.description != null && b.description !== '') return b.description
  if (b.reverts_batch_id != null) return `(#${b.reverts_batch_id} の巻き戻し)`
  return `(${b.kind ?? '不明'})`
}

/** 状態列。反映中は「applying 640/980」、partial は conflict / failed の件数を添える */
export function stateLabel(b: HistoryItem): string {
  if (b.state === 'prepared' || b.state === 'applying') {
    const done = b.applied + b.conflict + b.failed
    return `${b.state} ${done}/${b.affected ?? done}`
  }
  if (b.state === 'partial') {
    const parts = [`conflict ${b.conflict}`]
    if (b.failed > 0) parts.push(`failed ${b.failed}`)
    return `partial (${parts.join(', ')})`
  }
  return b.state
}

export function isTerminal(b: HistoryItem): boolean {
  return b.state !== 'prepared' && b.state !== 'applying'
}

/** [巻き戻す] は終端状態で、applied の op があり、全件戻し済みでないときだけ */
export function canRevert(b: HistoryItem): boolean {
  return isTerminal(b) && b.applied > 0 && b.reverted_at == null
}

/** [キャンセル] は prepared / applying のみ */
export function canCancel(b: HistoryItem): boolean {
  return !isTerminal(b)
}

/** 戻し済みの注記。戻し済みでなければ null */
export function revertedNote(b: HistoryItem): string | null {
  if (b.reverted_at == null) return null
  return b.reverted_by != null ? `#${b.reverted_by} で戻し済み` : '戻し済み'
}

/** epoch 秒 → "MM-DD HH:MM"（ローカル時刻） */
export function formatDateTime(epoch: number): string {
  const d = new Date(epoch * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`
}

/** edits の値（文字列配列 / 文字列 / 数値 / null）を 1 行の表示に */
export function formatValue(v: unknown): string {
  if (v == null) return '(なし)'
  if (Array.isArray(v)) return v.map((x) => String(x)).join(' / ')
  return String(v)
}
