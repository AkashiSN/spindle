import { describe, expect, it } from 'vitest'
import { filterToParam, sameFilter, toggleSort, tracksUrl, DEFAULT_SORT } from './filter'

describe('filterToParam', () => {
  it('空は空文字、キー順と flags 順を固定する', () => {
    expect(filterToParam({})).toBe('')
    expect(filterToParam({ q: '  ' })).toBe('')
    expect(filterToParam({ flags: ['pending', 'missing', 'pending'], category: 'J-Pop' })).toBe(
      '{"category":"J-Pop","flags":["missing","pending"]}',
    )
    expect(sameFilter({ flags: ['missing', 'pending'] }, { flags: ['pending', 'missing'] })).toBe(true)
    expect(filterToParam({ album_id: 0 })).toBe('{"album_id":0}')
  })

  it('tracksUrl は filter を URL エンコードし cursor を付ける', () => {
    const u = tracksUrl({ filter: { q: '情緒' }, sort: { key: 'title', desc: true }, cursor: 'abc' })
    const p = new URL(u, 'http://x').searchParams
    expect(p.get('filter')).toBe('{"q":"情緒"}')
    expect(p.get('sort')).toBe('-title')
    expect(p.get('cursor')).toBe('abc')
    expect(p.get('limit')).toBe('500')
    expect(new URL(tracksUrl({ filter: {}, sort: DEFAULT_SORT }), 'http://x').searchParams.has('filter')).toBe(
      false,
    )
  })

  it('toggleSort は同じ列で反転、別の列で昇順', () => {
    expect(toggleSort(DEFAULT_SORT, 'album')).toEqual({ key: 'album', desc: true })
    expect(toggleSort({ key: 'album', desc: true }, 'title')).toEqual({ key: 'title', desc: false })
  })
})
