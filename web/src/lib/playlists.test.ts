import { describe, expect, it } from 'vitest'
import { DEFAULT_SORT } from './filter'
import {
  dropTarget,
  exportNotice,
  parseDragIds,
  scopeAfterPlaylistDelete,
  serializeDragIds,
  sortForScope,
  TRACK_DRAG_TYPE,
} from './playlists'

describe('sortForScope', () => {
  it('プレイリストに入ると position、出ると既定に戻る', () => {
    expect(sortForScope({}, { playlist_id: 3 }, DEFAULT_SORT)).toEqual({ key: 'position', desc: false })
    expect(sortForScope({ playlist_id: 3 }, { album_id: 1 }, { key: 'position', desc: false })).toEqual(DEFAULT_SORT)
    expect(sortForScope({ playlist_id: 3 }, {}, { key: 'position', desc: true })).toEqual(DEFAULT_SORT)
  })

  it('プレイリスト間の移動と、プレイリスト内で別ソートにしていたときは触らない', () => {
    expect(sortForScope({ playlist_id: 3 }, { playlist_id: 4 }, { key: 'title', desc: true })).toEqual({
      key: 'title',
      desc: true,
    })
    expect(sortForScope({ album_id: 1 }, { album_id: 2 }, { key: 'title', desc: false })).toEqual({
      key: 'title',
      desc: false,
    })
  })

  it('同じプレイリストに flags を足しても position のまま', () => {
    expect(
      sortForScope({ playlist_id: 3 }, { playlist_id: 3, flags: ['missing'] }, { key: 'position', desc: false }),
    ).toEqual({ key: 'position', desc: false })
  })
})

describe('scopeAfterPlaylistDelete', () => {
  it('表示中のプレイリストを消したら「すべて」へ戻す。flags も持ち越さない', () => {
    expect(scopeAfterPlaylistDelete({ playlist_id: 3, flags: ['missing'] }, 3)).toEqual({})
  })
  it('別のプレイリスト・プレイリスト以外の scope は触らない', () => {
    const s = { playlist_id: 3 }
    expect(scopeAfterPlaylistDelete(s, 4)).toBe(s)
    const t = { album_id: 1 }
    expect(scopeAfterPlaylistDelete(t, 3)).toBe(t)
  })
})

describe('drag payload', () => {
  it('id 列を往復できる。壊れた入力は null', () => {
    expect(TRACK_DRAG_TYPE).toBe('application/x-spindle-tracks')
    expect(parseDragIds(serializeDragIds([3, 1, 2]))).toEqual([3, 1, 2])
    expect(parseDragIds('')).toBeNull()
    expect(parseDragIds('{"x":1}')).toBeNull()
    expect(parseDragIds('[1,"a"]')).toBeNull()
    expect(parseDragIds('[]')).toBeNull()
  })
})

describe('dropTarget', () => {
  const order = [10, 20, 30, 40, 50]
  it('行の上半分ならその行の前、下半分なら次の行の前', () => {
    expect(dropTarget(order, [10], 30, 'above')).toEqual({ before: 30 })
    expect(dropTarget(order, [10], 30, 'below')).toEqual({ before: 40 })
    expect(dropTarget(order, [10], 50, 'below')).toEqual({ before: null })
  })
  it('移動先が移動する集合の中なら、集合の外の次の行へ倒す', () => {
    expect(dropTarget(order, [20, 30], 40, 'below')).toEqual({ before: 50 })
    expect(dropTarget(order, [40, 50], 20, 'above')).toEqual({ before: 20 })
    // 集合の中に落としても、ずらした先が今の位置なら並びは変わらない
    expect(dropTarget(order, [20, 30], 30, 'above')).toBeNull()
    expect(dropTarget(order, [20, 30], 20, 'above')).toBeNull()
    expect(dropTarget(order, [40, 50], 50, 'below')).toBeNull()
    expect(dropTarget(order, [10, 20, 30, 40, 50], 30, 'above')).toBeNull()
  })
  it('順が変わらない落とし方は null', () => {
    expect(dropTarget(order, [20], 20, 'above')).toBeNull()
    expect(dropTarget(order, [20], 10, 'below')).toBeNull()
    expect(dropTarget(order, [20], 30, 'above')).toBeNull()
    expect(dropTarget(order, [50], 50, 'below')).toBeNull()
  })
  it('表示中に無い行へは落とせない', () => {
    expect(dropTarget(order, [10], 99, 'above')).toBeNull()
  })
})

describe('exportNotice', () => {
  it('件数と除外・タグ追随待ちを添える', () => {
    expect(exportNotice({ out_path: 'internal/通勤.m3u8', count: 3, skipped_missing: 0, stale_tags: 0 })).toBe(
      'Playlists/internal/通勤.m3u8 に 3 件を書き出し',
    )
    expect(exportNotice({ out_path: 'android/通勤.m3u8', count: 3, skipped_missing: 1, stale_tags: 2 })).toBe(
      'Playlists/android/通勤.m3u8 に 3 件を書き出し（missing 1 件は除外、タグ追随待ちの Derived 2 件を含む）',
    )
    expect(exportNotice({ out_path: 'android/x.m3u8', count: 1, skipped_missing: 0, stale_tags: 1 })).toBe(
      'Playlists/android/x.m3u8 に 1 件を書き出し（タグ追随待ちの Derived 1 件を含む）',
    )
  })
})
