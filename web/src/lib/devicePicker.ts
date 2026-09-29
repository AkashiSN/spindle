// ライブラリの左サイドバーのプレイリストの行から、端末の選曲（device_playlists）を触る（仕様 ④「選曲の近道」）

import type { Device } from '../api/types'

/** そのプレイリストに印が付いた端末（名前順） */
export function chipsFor(playlistId: number, devices: readonly Device[]): Device[] {
  return devices.filter((d) => d.playlist_ids.includes(playlistId)).sort((a, b) => a.name.localeCompare(b.name, 'ja'))
}

/** 印を付け外しした後の playlist_ids（昇順） */
export function togglePlaylist(d: Device, playlistId: number): number[] {
  const set = new Set(d.playlist_ids)
  if (set.has(playlistId)) set.delete(playlistId)
  else set.add(playlistId)
  return [...set].sort((a, b) => a - b)
}

/** 一覧の 1 台の playlist_ids だけを差し替える（PUT の完了を待たずに画面と次の切り替えへ反映するため） */
export function withPlaylistIds(items: readonly Device[] | null, id: number, playlistIds: number[]): Device[] | null {
  if (items == null) return null
  return items.map((d) => (d.id === id ? { ...d, playlist_ids: playlistIds } : d))
}

/** 取得した一覧に、送信待ち・送信中の端末の望む playlist_ids を重ねる（取り直しが楽観更新を古い値で潰さないように） */
export function mergePending(items: readonly Device[], pending: ReadonlyMap<number, number[]>): Device[] {
  if (pending.size === 0) return [...items]
  return items.map((d) => {
    const ids = pending.get(d.id)
    return ids ? { ...d, playlist_ids: ids } : d
  })
}

/** 選曲タブの印の下書きを付け外しする。`draft` が null（未編集）なら保存済みの `saved` から始める。
 *  結果が保存済みと同じになったら null（未編集）に戻す（その後のサイドバーでの付け外しに追随させる） */
export function toggleDraft(
  draft: readonly number[] | null,
  saved: readonly number[],
  playlistId: number,
  on: boolean,
): number[] | null {
  const base = draft ?? saved
  const next = on ? (base.includes(playlistId) ? [...base] : [...base, playlistId]) : base.filter((x) => x !== playlistId)
  return sameIdSet(next, saved) ? null : next
}

/** 順序を問わず同じ集合か */
export function sameIdSet(a: readonly number[], b: readonly number[]): boolean {
  if (a.length !== b.length) return false
  const s = new Set(a)
  return b.every((x) => s.has(x))
}
