import { describe, expect, it } from 'vitest'
import { overlaysOf, withOverlay } from './backClose'
import { sheetFromHistory } from './track'

describe('overlaysOf / withOverlay', () => {
  it('history.state の overlay（文字列の配列）を読む。形が違えば空', () => {
    expect(overlaysOf(null)).toEqual([])
    expect(overlaysOf({ sheet: 3 })).toEqual([])
    expect(overlaysOf({ overlay: 'x' })).toEqual([])
    expect(overlaysOf({ overlay: ['a', 1, 'b'] })).toEqual(['a', 'b'])
  })
  it('積むときは曲のシート（sheet / edit）を引き継ぐので、戻ってもシートは開いたまま', () => {
    const s = withOverlay({ sheet: 7, edit: 'title' }, 'a')
    expect(overlaysOf(s)).toEqual(['a'])
    expect(sheetFromHistory(s)).toEqual({ trackId: 7, editKey: 'title' })
    expect(overlaysOf(withOverlay(s, 'b'))).toEqual(['a', 'b'])
    expect(overlaysOf(withOverlay(null, 'a'))).toEqual(['a'])
  })
})
