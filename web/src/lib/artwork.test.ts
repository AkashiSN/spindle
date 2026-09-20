import { describe, expect, it } from 'vitest'
import type { AlbumRow } from '../api/types'
import { albumTitle, artworkUrl, displayArtworkHash, gridAlbums, parsePictureValue, uploadedSummary, albumCountLabel, albumsUrl } from './artwork'

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
  album_gain: false,
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

describe('parsePictureValue', () => {
  it('<mime>:<sha256hex> を分解する', () => {
    const hash = 'ab'.repeat(32)
    expect(parsePictureValue(`image/jpeg:${hash}`)).toEqual({ mime: 'image/jpeg', hash })
  })
  it('形が違えば null', () => {
    expect(parsePictureValue('image/jpeg:short')).toBeNull()
    expect(parsePictureValue(':' + 'a'.repeat(64))).toBeNull()
    expect(parsePictureValue('ab'.repeat(32))).toBeNull()
    expect(parsePictureValue(null)).toBeNull()
    expect(parsePictureValue(['image/jpeg:' + 'a'.repeat(64)])).toBeNull()
  })
})

describe('uploadedSummary', () => {
  it('形式・寸法・サイズ（KiB は切り上げ、MiB は小数 1 桁）', () => {
    expect(uploadedSummary({ sha256: '', mime: 'image/jpeg', width: 1400, height: 1400, bytes: 250_000 })).toBe(
      'JPEG 1400×1400 245 KiB',
    )
    expect(uploadedSummary({ sha256: '', mime: 'image/png', width: 3000, height: 3000, bytes: 5 * 1024 * 1024 })).toBe(
      'PNG 3000×3000 5.0 MiB',
    )
  })
})

describe('displayArtworkHash', () => {
  it('トラック自身の画像を優先し、無ければ album、どちらも無ければ null', () => {
    expect(displayArtworkHash({ artwork_hash: 't' }, { artwork_hash: 'a' })).toBe('t')
    expect(displayArtworkHash({ artwork_hash: null }, { artwork_hash: 'a' })).toBe('a')
    expect(displayArtworkHash(null, { artwork_hash: 'a' })).toBe('a')
    expect(displayArtworkHash({ artwork_hash: null }, null)).toBeNull()
  })
})

describe('albums filter (P4-6)', () => {
  it('albumsUrl は空なら全件、あれば filter を URL エンコードして付ける', () => {
    expect(albumsUrl('')).toBe('/api/albums')
    expect(albumsUrl('{"q":"花譜","album_ids":[1,2]}')).toBe(
      '/api/albums?filter=%7B%22q%22%3A%22%E8%8A%B1%E8%AD%9C%22%2C%22album_ids%22%3A%5B1%2C2%5D%7D',
    )
  })

  it('albumCountLabel は絞り込み中だけ「全 M 件中」', () => {
    expect(albumCountLabel(721, 721, false)).toBe('721 件')
    expect(albumCountLabel(12, 1234, true)).toBe('全 1,234 件中 12 件')
    expect(albumCountLabel(0, 5, true)).toBe('全 5 件中 0 件')
  })
})
