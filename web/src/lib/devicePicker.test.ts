import { describe, expect, it } from 'vitest'
import type { Device } from '../api/types'
import { chipsFor, togglePlaylist, withPlaylistIds } from './devicePicker'

const d = (id: number, name: string, playlist_ids: number[]): Device => ({
  id, name, transport: 'agent', variant: 'aac', selection: 'playlists', generation: 1, connected: null,
  counts: { add: 0, update: 0, move: 0, delete: 0, waiting: 0, error: 0, synced: 0 },
  last_synced_at: null, playlist_ids, open_plan: false,
})

describe('選曲の近道', () => {
  it('チップはその印が付いた端末だけ、名前順', () => {
    const got = chipsFor(5, [d(1, 'Xperia', [5]), d(2, 'iPhone', [5, 6]), d(3, 'iPad', [6])])
    expect(got.map((x) => x.name)).toEqual(['iPhone', 'Xperia'])
  })
  it('印を付け外しする', () => {
    expect(togglePlaylist(d(1, 'a', [6, 2]), 5)).toEqual([2, 5, 6])
    expect(togglePlaylist(d(1, 'a', [5, 6]), 5)).toEqual([6])
  })
  it('楽観更新: 1 台の playlist_ids だけ差し替え、続けての切り替えが前の印を落とさない', () => {
    const items = [d(1, 'a', []), d(2, 'b', [9])]
    const after = withPlaylistIds(items, 1, togglePlaylist(items[0], 5))
    const next = withPlaylistIds(after, 1, togglePlaylist(after![0], 6))
    expect(next!.map((x) => x.playlist_ids)).toEqual([[5, 6], [9]])
    expect(withPlaylistIds(null, 1, [1])).toBeNull()
  })
})
