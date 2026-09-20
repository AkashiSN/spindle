import { describe, expect, it } from 'vitest'
import type { AlbumRow } from '../api/types'
import { albumsOfRows } from './albumGain'

const album = (id: number): AlbumRow => ({
  id,
  rel_dir: `A/${id}`,
  category: null,
  albumartist: 'aa',
  album: `al${id}`,
  date: null,
  original_date: null,
  edition: null,
  mb_release_id: null,
  disc_count: null,
  artwork_id: null,
  artwork_hash: null,
  track_count: 1,
  duration_ms: 0,
  missing_since: null,
  album_gain: false,
})

describe('albumsOfRows', () => {
  it('選択行の album を出現順・重複なしで返し、一覧に無い id と album 無しの行は落とす', () => {
    const albums = [album(1), album(2), album(3)]
    const rows = [{ album_id: 2 }, { album_id: null }, { album_id: 2 }, { album_id: 9 }, { album_id: 1 }]
    expect(albumsOfRows(rows, albums).map((a) => a.id)).toEqual([2, 1])
  })
  it('空なら空', () => {
    expect(albumsOfRows([], [album(1)])).toEqual([])
  })
})
