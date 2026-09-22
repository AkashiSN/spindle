import { describe, expect, it } from 'vitest'
import { Latest } from './latest'

describe('Latest', () => {
  it('最新の要求だけが current', () => {
    const g = new Latest()
    const a = g.next()
    const b = g.next()
    expect(g.isCurrent(a)).toBe(false)
    expect(g.isCurrent(b)).toBe(true)
  })
  it('invalidate で進行中の要求は全部捨てる', () => {
    const g = new Latest()
    const a = g.next()
    g.invalidate()
    expect(g.isCurrent(a)).toBe(false)
    const b = g.next()
    expect(g.isCurrent(b)).toBe(true)
  })
})
