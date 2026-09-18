import { describe, expect, it } from 'vitest'
import { formatArtistAlbum, formatDuration, formatTitleArtist, formatTrackNo } from './format'

describe('format', () => {
  it('duration', () => {
    expect(formatDuration(null)).toBe('')
    expect(formatDuration(280_000)).toBe('4:40')
    expect(formatDuration(3_725_000)).toBe('1:02:05')
    expect(formatDuration(59_499)).toBe('0:59')
  })
  it('track no', () => {
    expect(formatTrackNo(null, null)).toBe('')
    expect(formatTrackNo(1, 3)).toBe('03')
    expect(formatTrackNo(null, 12)).toBe('12')
    expect(formatTrackNo(2, 3)).toBe('2-03')
  })

  it('foobar の Artist/album と Title / track artist（D-58）', () => {
    expect(formatArtistAlbum({ albumartist: 'A', album: 'X' })).toBe('A - X')
    expect(formatArtistAlbum({ albumartist: 'A', album: null })).toBe('A')
    expect(formatArtistAlbum({ albumartist: null, album: 'X' })).toBe('X')
    expect(formatArtistAlbum({ albumartist: null, album: null })).toBe('')
    expect(formatTitleArtist({ title: 'T', artist_display: 'B', albumartist: 'A' })).toBe('T // B')
    expect(formatTitleArtist({ title: 'T', artist_display: 'A', albumartist: 'A' })).toBe('T')
    expect(formatTitleArtist({ title: 'T', artist_display: null, albumartist: 'A' })).toBe('T')
    expect(formatTitleArtist({ title: null, artist_display: 'B', albumartist: null })).toBe(' // B')
  })
})
