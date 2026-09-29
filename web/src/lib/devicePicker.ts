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
