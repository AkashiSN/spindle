import { describe, expect, it } from 'vitest'
import { formatDuration, formatTrackNo } from './format'

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
})
