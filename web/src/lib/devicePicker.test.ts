import { describe, expect, it } from 'vitest'
import type { Device } from '../api/types'
import { chipsFor, mergePending, sameIdSet, toggleDraft, togglePlaylist, withPlaylistIds } from './devicePicker'

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
  it('取り直した一覧に、送信中の端末の望む印を重ねる', () => {
    const fetched = [d(1, 'a', [5]), d(2, 'b', [9])]
    const merged = mergePending(fetched, new Map([[1, [5, 6]]]))
    expect(merged.map((x) => x.playlist_ids)).toEqual([[5, 6], [9]])
    expect(mergePending(fetched, new Map()).map((x) => x.playlist_ids)).toEqual([[5], [9]])
  })
})

describe('選曲タブの下書き', () => {
  it('未編集なら保存済みから始め、保存済みと同じに戻れば未編集（null）に戻る', () => {
    expect(toggleDraft(null, [5], 6, true)).toEqual([5, 6])
    expect(toggleDraft([5, 6], [5], 6, false)).toBeNull()
    expect(toggleDraft(null, [5], 5, true)).toBeNull()
  })
  it('編集中はサイドバーの変更（保存済みの値）ではなく下書きを使う', () => {
    const draft = toggleDraft(null, [5], 7, true)
    expect(draft).toEqual([5, 7])
    // サイドバーで 6 を付けた後（保存済みが [5, 6]）も下書きは変わらない
    expect(toggleDraft(draft, [5, 6], 8, true)).toEqual([5, 7, 8])
  })
  it('順序を問わず同じ集合か', () => {
    expect(sameIdSet([1, 2], [2, 1])).toBe(true)
    expect(sameIdSet([1, 2], [1])).toBe(false)
  })
})
