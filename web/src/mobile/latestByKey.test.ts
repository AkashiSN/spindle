import { describe, expect, it } from 'vitest'
import { LatestByKey } from './latestByKey'

describe('LatestByKey', () => {
  it('同じキーでは最新の要求だけが current（後から返った古い応答は捨てる）', () => {
    const g = new LatestByKey<number>()
    const a = g.next(1)
    const b = g.next(1)
    expect(g.isCurrent(1, b)).toBe(true)
    expect(g.isCurrent(1, a)).toBe(false)
  })
  it('キーが違えば互いに無効にしない', () => {
    const g = new LatestByKey<number>()
    const a = g.next(1)
    const b = g.next(2)
    expect(g.isCurrent(1, a)).toBe(true)
    expect(g.isCurrent(2, b)).toBe(true)
  })
  it('要求していないキーは current でない', () => {
    const g = new LatestByKey<number>()
    expect(g.isCurrent(3, 1)).toBe(false)
  })
  it('invalidate ですべてのキーの進行中の要求を捨てる', () => {
    const g = new LatestByKey<number>()
    const a = g.next(1)
    const b = g.next(2)
    g.invalidate()
    expect(g.isCurrent(1, a)).toBe(false)
    expect(g.isCurrent(2, b)).toBe(false)
    const c = g.next(1)
    expect(g.isCurrent(1, c)).toBe(true)
  })
})
