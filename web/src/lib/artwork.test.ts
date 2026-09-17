import { describe, expect, it } from 'vitest'
import type { AlbumRow } from '../api/types'
import { albumTitle, artworkUrl, gridAlbums } from './artwork'

const base: AlbumRow = {
  id: 1,
  rel_dir: 'J-Pop/Artist/Album',
  category: 'J-Pop',
  albumartist: 'Artist',
  album: 'Album',
  date: '2024',
  original_date: null,
  edition: null,
  mb_release_id: null,
  disc_count: null,
  artwork_id: 7,
  artwork_hash: 'ab'.repeat(32),
  track_count: 12,
  duration_ms: 2_800_000,
  missing_since: null,
}

describe('artwork', () => {
  it('url', () => {
    expect(artworkUrl('ab'.repeat(32))).toBe(`/api/artwork/${'ab'.repeat(32)}`)
    expect(artworkUrl('ab'.repeat(32), 256)).toBe(`/api/artwork/${'ab'.repeat(32)}?size=256`)
    expect(artworkUrl('a/b', 768)).toBe('/api/artwork/a%2Fb?size=768')
  })
  it('grid hides missing albums and keeps order', () => {
    const rows = [base, { ...base, id: 2, missing_since: 1 }, { ...base, id: 3, artwork_hash: null }]
    expect(gridAlbums(rows).map((a) => a.id)).toEqual([1, 3])
  })
  it('title falls back to the directory name', () => {
    expect(albumTitle(base)).toBe('Album')
    expect(albumTitle({ ...base, album: null })).toBe('Album')
    expect(albumTitle({ ...base, album: null, rel_dir: 'X/Y/Dir Name' })).toBe('Dir Name')
    expect(albumTitle({ ...base, album: '', rel_dir: 'Solo' })).toBe('Solo')
  })
})
