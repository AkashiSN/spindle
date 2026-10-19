import { describe, expect, it } from 'vitest'
import type { AlbumRow } from '../api/types'
import { albumsOf, artistsOf, nextPageNeeded, refetchLimits } from './library'

const a = (id: number, albumartist: string | null, album: string, extra: Partial<AlbumRow> = {}): AlbumRow =>
  ({
    id,
    rel_dir: `J-Pop/${albumartist ?? ''}/${album}`,
    category: 'J-Pop',
    albumartist,
    album,
    date: null,
    original_date: null,
    edition: null,
    mb_release_id: null,
    disc_count: null,
    artwork_id: null,
    artwork_hash: null,
    track_count: 10,
    duration_ms: 0,
    missing_since: null,
    album_gain: false,
    ...extra,
  }) as AlbumRow

describe('library', () => {
  it('artistsOf は albumartist ごとに数え、missing を除き、検索で絞る', () => {
    const xs = [
      a(1, 'YOASOBI', 'THE BOOK'),
      a(2, 'YOASOBI', 'THE BOOK 2'),
      a(3, 'Vaundy', 'strobo'),
      a(4, 'Aimer', 'Walpurgis', { missing_since: 1 }),
    ]
    expect(artistsOf(xs, '')).toEqual([
      { name: 'Vaundy', albumCount: 1, trackCount: 10 },
      { name: 'YOASOBI', albumCount: 2, trackCount: 20 },
    ])
    expect(artistsOf(xs, 'yoa').map((e) => e.name)).toEqual(['YOASOBI'])
  })
  it('albumartist が無いアルバムは（なし）にまとめる', () => {
    expect(artistsOf([a(1, null, 'x')], '')[0]!.name).toBe('（なし）')
  })
  it('albumsOf は albumartist と検索語（アルバム名・albumartist）で絞る', () => {
    const xs = [a(1, 'YOASOBI', 'THE BOOK'), a(3, 'Vaundy', 'strobo')]
    expect(albumsOf(xs, { albumartist: 'YOASOBI' }).map((x) => x.id)).toEqual([1])
    expect(albumsOf(xs, { q: 'STRO' }).map((x) => x.id)).toEqual([3])
  })
  it('albumsOf は（なし）で albumartist が無いアルバムを返し、missing を除く', () => {
    const xs = [a(1, null, 'x'), a(2, 'Vaundy', 'strobo'), a(3, 'Vaundy', 'replica', { missing_since: 1 })]
    expect(albumsOf(xs, { albumartist: '（なし）' }).map((x) => x.id)).toEqual([1])
    expect(albumsOf(xs, {}).map((x) => x.id)).toEqual([1, 2])
  })
  it('nextPageNeeded は末尾 50 行以内かつ続きがあるときだけ', () => {
    expect(nextPageNeeded(449, 500, true)).toBe(true)
    expect(nextPageNeeded(400, 500, true)).toBe(false)
    expect(nextPageNeeded(499, 500, false)).toBe(false)
  })

  it('refetchLimits は読み込み済みの行数（最低 500）をサーバの上限 1000 で区切って覆う', () => {
    expect(refetchLimits(0)).toEqual([500])
    expect(refetchLimits(120)).toEqual([500])
    expect(refetchLimits(500)).toEqual([500])
    expect(refetchLimits(1000)).toEqual([1000])
    expect(refetchLimits(1500)).toEqual([1000, 500])
    expect(refetchLimits(2000)).toEqual([1000, 1000])
  })
})
