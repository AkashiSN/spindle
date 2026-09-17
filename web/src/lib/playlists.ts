// プレイリスト UI の純粋ロジック（SPEC §12.1、P1-6、D-53）。
//
// - スコープがプレイリストに入ったら表のソートを position に、出たら既定に戻す
// - 表の行 → サイドバーのプレイリストへのドラッグは id 列を dataTransfer に載せる
// - プレイリスト内の並べ替えは「落とした行の前 / 次の行の前」を移動先（before）に写す

import type { ExportResponse } from '../api/types'
import { DEFAULT_SORT, type Filter, type Sort } from './filter'

/** dataTransfer の MIME。表の行だけがこの型を載せる */
export const TRACK_DRAG_TYPE = 'application/x-spindle-tracks'

/** 書き出し結果の通知文。missing の除外と delivery のタグ追随待ち（P1-8）を添える */
export function exportNotice(r: ExportResponse): string {
  const extra: string[] = []
  if (r.skipped_missing > 0) extra.push(`missing ${r.skipped_missing} 件は除外`)
  if (r.stale_tags > 0) extra.push(`タグ追随待ちの Derived ${r.stale_tags} 件を含む`)
  return `Playlists/${r.out_path} に ${r.count} 件を書き出し${extra.length > 0 ? `（${extra.join('、')}）` : ''}`
}

export function sortForScope(prev: Omit<Filter, 'q'>, next: Omit<Filter, 'q'>, sort: Sort): Sort {
  const entering = next.playlist_id != null && prev.playlist_id !== next.playlist_id
  const leaving = prev.playlist_id != null && next.playlist_id == null
  if (entering && prev.playlist_id == null) return { key: 'position', desc: false }
  if (leaving && sort.key === 'position') return DEFAULT_SORT
  return sort
}

/** プレイリストを消したとき、表がそれを表示していたら scope を「すべて」に戻す（それ以外は同じ値） */
export function scopeAfterPlaylistDelete<S extends Omit<Filter, 'q'>>(scope: S, deletedId: number): S | Record<string, never> {
  return scope.playlist_id === deletedId ? {} : scope
}

export function serializeDragIds(ids: readonly number[]): string {
  return JSON.stringify(ids)
}

export function parseDragIds(text: string): number[] | null {
  if (!text) return null
  let v: unknown
  try {
    v = JSON.parse(text)
  } catch {
    return null
  }
  if (!Array.isArray(v) || v.length === 0) return null
  if (!v.every((x) => typeof x === 'number' && Number.isInteger(x))) return null
  return v as number[]
}

export type DropHalf = 'above' | 'below'

/**
 * 並べ替えの移動先。`order` は表示中の並び（position 順）、`dragged` は動かす行、`overId` は
 * 落とした行、`half` はその行のどちら側か。移動先が動かす集合の中に落ちたら集合の外の次の行へ
 * ずらす。結果として並びが変わらないときと、移動先が決められないときは null
 */
export function dropTarget(
  order: readonly number[],
  dragged: readonly number[],
  overId: number,
  half: DropHalf,
): { before: number | null } | null {
  const moving = new Set(dragged)
  const overIdx = order.indexOf(overId)
  if (overIdx < 0) return null
  // 移動先 index（この index の行の前に入れる。order.length なら末尾）
  let idx = half === 'above' ? overIdx : overIdx + 1
  while (idx < order.length && moving.has(order[idx])) idx++
  const before = idx < order.length ? order[idx] : null
  if (before == null && order.every((id) => moving.has(id))) return null
  // 動かした後の並びが今と同じなら何もしない
  const rest = order.filter((id) => !moving.has(id))
  const movedInOrder = order.filter((id) => moving.has(id))
  const at = before == null ? rest.length : rest.indexOf(before)
  const next = [...rest.slice(0, at), ...movedInOrder, ...rest.slice(at)]
  if (next.length === order.length && next.every((id, i) => id === order[i])) return null
  return { before }
}
