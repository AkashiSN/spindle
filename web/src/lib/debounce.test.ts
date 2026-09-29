import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest'
import { TrailingDebounce } from './debounce'

describe('TrailingDebounce', () => {
  beforeEach(() => {
    vi.useFakeTimers()
  })
  afterEach(() => {
    vi.useRealTimers()
  })

  it('静かになってから waitMs 後に 1 回だけ実行する', () => {
    const fn = vi.fn()
    const d = new TrailingDebounce(fn, 3000, 10_000)
    d.trigger()
    vi.advanceTimersByTime(2000)
    d.trigger()
    vi.advanceTimersByTime(2999)
    expect(fn).not.toHaveBeenCalled()
    vi.advanceTimersByTime(1)
    expect(fn).toHaveBeenCalledTimes(1)
    vi.advanceTimersByTime(10_000)
    expect(fn).toHaveBeenCalledTimes(1)
  })

  it('呼ばれ続けても最初の呼び出しから maxWaitMs で実行する', () => {
    const fn = vi.fn()
    const d = new TrailingDebounce(fn, 3000, 10_000)
    for (let i = 0; i < 40; i++) {
      d.trigger()
      vi.advanceTimersByTime(250)
    }
    // 250ms × 40 = 10 秒。延ばし続けても 10 秒で 1 回
    expect(fn).toHaveBeenCalledTimes(1)
  })

  it('cancel は予約を捨て、flush は今すぐ実行する', () => {
    const fn = vi.fn()
    const d = new TrailingDebounce(fn, 3000, 10_000)
    d.trigger()
    d.cancel()
    vi.advanceTimersByTime(20_000)
    expect(fn).not.toHaveBeenCalled()
    d.trigger()
    d.flush()
    expect(fn).toHaveBeenCalledTimes(1)
    vi.advanceTimersByTime(20_000)
    expect(fn).toHaveBeenCalledTimes(1)
  })
})
