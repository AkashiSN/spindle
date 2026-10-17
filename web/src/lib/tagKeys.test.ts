import { describe, expect, it } from 'vitest'
import { filterTagKeys, mergeTagKeys, moveActive } from './tagKeys'

const lib = [
  { key: 'TITLE', tracks: 9000 },
  { key: 'TRACKNUMBER', tracks: 8000 },
  { key: 'TRACK_TYPE', tracks: 2000 },
  { key: 'TRACKTOTAL', tracks: 7000 },
  { key: 'LYRICIST', tracks: 3 },
]

describe('mergeTagKeys', () => {
  it('ライブラリのキーを先に（曲数の順のまま）、よく使うキーで無いものを 0 曲で後ろに足す', () => {
    expect(mergeTagKeys(lib, ['TITLE', 'COMPOSER', 'GENRE'])).toEqual([...lib, { key: 'COMPOSER', tracks: 0 }, { key: 'GENRE', tracks: 0 }])
  })
})

describe('filterTagKeys', () => {
  it('前方一致（大小文字を区別しない）で、並びは入力の順（曲数の順）のまま', () => {
    expect(filterTagKeys(lib, 'tr').map((k) => k.key)).toEqual(['TRACKNUMBER', 'TRACK_TYPE', 'TRACKTOTAL'])
    expect(filterTagKeys(lib, ' TRACK_').map((k) => k.key)).toEqual(['TRACK_TYPE'])
  })
  it('空の入力では出さない。部分一致（前方以外）は出さない', () => {
    expect(filterTagKeys(lib, '')).toEqual([])
    expect(filterTagKeys(lib, '  ')).toEqual([])
    expect(filterTagKeys(lib, 'ICIST')).toEqual([])
  })
  it('既にあるキー（列・フィールド）は除き、上限で切る', () => {
    expect(filterTagKeys(lib, 't', { exclude: ['TITLE', 'TRACKTOTAL'] }).map((k) => k.key)).toEqual(['TRACKNUMBER', 'TRACK_TYPE'])
    expect(filterTagKeys(lib, 't', { limit: 2 }).map((k) => k.key)).toEqual(['TITLE', 'TRACKNUMBER'])
  })
  it('入力と完全に同じキーだけになったら出さない（選び終えている）', () => {
    expect(filterTagKeys(lib, 'lyricist')).toEqual([])
    expect(filterTagKeys(lib, 'TRACK_TYPE')).toEqual([])
  })
})

describe('moveActive', () => {
  it('↓ で先頭から、↑ で入力欄（-1）へ戻り、端では止まる', () => {
    expect(moveActive(-1, 3, 1)).toBe(0)
    expect(moveActive(2, 3, 1)).toBe(2)
    expect(moveActive(0, 3, -1)).toBe(-1)
    expect(moveActive(-1, 3, -1)).toBe(-1)
    expect(moveActive(5, 0, 1)).toBe(-1)
  })
})
