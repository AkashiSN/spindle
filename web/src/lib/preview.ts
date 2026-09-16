// プレビュー結果の索引と、表のセルへの差分表示・インライン編集の写像（SPEC §12.2 / §12.3）

import type { PreviewResponse, TagChange } from '../api/types'
import type { Selection } from './selection'
import { toSelectionBody } from './selection'

export type PreviewCounts = { count: number; changed: number; unchanged: number; pending_excluded: number }

export type PreviewState = {
  token: string
  /** previewKey()。選択・操作・ソートが変わったら結果は古い */
  key: string
  counts: PreviewCounts
  changesById: ReadonlyMap<number, Record<string, TagChange>>
}

export function indexPreview(res: PreviewResponse, key: string): PreviewState {
  const changesById = new Map<number, Record<string, TagChange>>()
  for (const it of res.items) changesById.set(it.id, it.changes)
  return {
    token: res.selection_token,
    key,
    counts: {
      count: res.count,
      changed: res.changed,
      unchanged: res.unchanged,
      pending_excluded: res.pending_excluded,
    },
    changesById,
  }
}

/** 選択・操作・ソートの同一性（プレビューがまだ有効かの判定） */
export function previewKey(sel: Selection, ops: readonly unknown[], sortParam: string): string {
  return JSON.stringify([toSelectionBody(sel), ops, sortParam])
}

/** 表の列 id → 編集するタグキー（差分表示とインライン編集の対象） */
export const COLUMN_TAG: Readonly<Record<string, string>> = {
  title: 'TITLE',
  artist: 'ARTIST',
  album: 'ALBUM',
  albumartist: 'ALBUMARTIST',
  date: 'DATE',
  no: 'TRACKNUMBER',
}

export function cellDiff(state: PreviewState | null, id: number, columnId: string): TagChange | null {
  if (!state) return null
  const key = COLUMN_TAG[columnId]
  if (!key) return null
  return state.changesById.get(id)?.[key] ?? null
}

export function formatValues(v: string[] | null): string {
  return v == null ? '' : v.join(', ')
}

/** セルのダブルクリック編集を 1 件バッチの操作にする。編集できない列・値なら null */
export function inlineEditOps(columnId: string, raw: string): Array<{ op: 'set'; key: string; value: string }> | null {
  const value = raw.trim()
  if (columnId === 'no') {
    const m = /^(?:(\d+)-)?(\d+)$/.exec(value)
    if (!m) return null
    const ops: Array<{ op: 'set'; key: string; value: string }> = []
    if (m[1] != null) ops.push({ op: 'set', key: 'DISCNUMBER', value: String(Number(m[1])) })
    ops.push({ op: 'set', key: 'TRACKNUMBER', value: String(Number(m[2])) })
    return ops
  }
  const key = COLUMN_TAG[columnId]
  if (!key) return null
  return [{ op: 'set', key, value }]
}

/** 409 `pending` の確認（「M 件を除外して適用 / 待つ」）。プレビューの key に紐づく */
export type PendingPrompt = { key: string; count: number; trackIds: number[] }

/**
 * 表示してよい pending の確認。選択・操作・ソートが変わってプレビューが古くなったら
 * （key が違う）出さない（古い件数と「除外して適用」を残さない）
 */
export function visiblePendingPrompt(prompt: PendingPrompt | null, currentKey: string): PendingPrompt | null {
  return prompt != null && prompt.key === currentKey ? prompt : null
}
