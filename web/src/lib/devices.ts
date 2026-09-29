// 端末タブの純粋ロジック（P5-2、D-95）。件数・差分表の並び・文言

import { ApiError } from '../api/client'
import type { Device, DeviceCounts, DeviceDiff, DiffItem, DiffOp } from '../api/types'
import { formatDateTime } from './history'

/** 未反映の件数（ナビのバッジ・一覧）。待ちはハッシュ計算などを待っているだけなので数えない */
export function unsyncedCount(c: DeviceCounts): number {
  return c.add + c.update + c.move + c.delete + c.error
}

/** 全端末の未反映の合計（同じ曲が 2 端末で未反映なら 2） */
export function totalBadge(devices: readonly Device[]): number {
  return devices.reduce((n, d) => n + unsyncedCount(d.counts), 0)
}

export const OP_LABELS: Record<DiffOp, string> = {
  add: '追加',
  update: '更新',
  move: '移動',
  update_move: '更新 + 移動',
  delete: '削除',
  waiting: '待ち',
  error: 'エラー',
}

/** 差分表の並び。手当てが要るもの（エラー）と端末から消えるもの（削除）を先に出す */
const ORDER: DiffOp[] = ['error', 'delete', 'update_move', 'move', 'update', 'add', 'waiting']

export function sortDiffItems(items: readonly DiffItem[]): DiffItem[] {
  return [...items].sort(
    (a, b) => ORDER.indexOf(a.op) - ORDER.indexOf(b.op) || (a.title ?? '').localeCompare(b.title ?? '', 'ja'),
  )
}

/** 同期で送る件数（待ち・エラー以外）と送る量 */
export function syncSummary(d: DeviceDiff): { count: number; bytes: number } {
  return {
    count: d.items.filter((i) => i.op !== 'waiting' && i.op !== 'error').length,
    bytes: d.estimate.transfer_bytes,
  }
}

/** スマートプレイリストの評価時刻の説明。評価待ち（ライブラリの変更をまだ反映していない）を優先する */
export function describeEvaluation(e: DeviceDiff['evaluations'][number], _now: number): string {
  if (e.pending) return `${e.name}: 評価待ち（ライブラリの変更を反映する前の結果）`
  if (e.evaluated_at == null) return `${e.name}: 未評価`
  return `${e.name}: ${formatDateTime(e.evaluated_at)} に評価`
}

/** 端末 API のエラーを日本語にする */
export function deviceMessage(e: unknown): string {
  if (e instanceof ApiError) {
    switch (e.code) {
      case 'open_plan':
        return '同期が途中か実行中なので変更できません（完了させるか、計画を破棄してから）'
      case 'cycle':
        return 'このスマートプレイリストは端末の状態（on_device / device_pending）を使っているので、端末の選曲に載せられません'
      case 'duplicate':
        return '同じ名前の端末があります'
      case 'not_found':
        return '端末が見つかりません（削除された可能性があります）'
    }
    return e.message
  }
  return e instanceof Error ? e.message : String(e)
}
