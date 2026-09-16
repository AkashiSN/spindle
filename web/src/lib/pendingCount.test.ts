import { describe, expect, it } from 'vitest'
import { PendingCounter, pendingCountUrl, type PendingCount } from './pendingCount'

describe('pendingCountUrl', () => {
  it('選択時のフィルタに pending を足し、limit=1 で数える', () => {
    const p = (key: string) => new URL(pendingCountUrl(key), 'http://x').searchParams
    expect(p('').get('filter')).toBe('{"flags":["pending"]}')
    expect(p('').get('limit')).toBe('1')
    expect(p('{"category":"J-Pop","flags":["missing"]}').get('filter')).toBe(
      '{"category":"J-Pop","flags":["missing","pending"]}',
    )
    expect(p('{"flags":["pending"]}').get('filter')).toBe('{"flags":["pending"]}')
    expect(p('not json').get('filter')).toBe('{"flags":["pending"]}')
  })
})

describe('PendingCounter', () => {
  type Req = { url: string; signal: AbortSignal; resolve: (v: { total: number }) => void }

  function harness() {
    const reqs: Req[] = []
    const changes: Array<PendingCount | null> = []
    const counter = new PendingCounter(
      (url, signal) => new Promise((resolve) => reqs.push({ url, signal, resolve })),
      (v) => changes.push(v),
    )
    return { reqs, changes, counter }
  }

  it('イベントのたびに取り直し、古い応答は新しい集計を上書きしない', async () => {
    const { reqs, changes, counter } = harness()
    counter.refresh('{}')
    reqs[0].resolve({ total: 10 })
    await Promise.resolve()
    expect(counter.current).toEqual({ key: '{}', total: 10 })

    // batch 完了 → 取り直し。その応答を待つ間にもう 1 回イベント（resync）
    counter.refresh('{}')
    counter.refresh('{}')
    expect(reqs[1].signal.aborted).toBe(true) // 追い越された取得は中断
    // 遅れて届いた古い応答（reqs[1]）は無視される
    reqs[1].resolve({ total: 999 })
    await Promise.resolve()
    expect(counter.current).toEqual({ key: '{}', total: 10 })
    reqs[2].resolve({ total: 3 })
    await Promise.resolve()
    expect(counter.current).toEqual({ key: '{}', total: 3 })
    expect(changes).toEqual([
      { key: '{}', total: 10 },
      { key: '{}', total: 3 },
    ])
  })

  it('選択のフィルタが変われば key も変わり、clear で null になる', async () => {
    const { reqs, changes, counter } = harness()
    counter.refresh('{"q":"x"}')
    reqs[0].resolve({ total: 2 })
    await Promise.resolve()
    expect(counter.current).toEqual({ key: '{"q":"x"}', total: 2 })
    counter.clear()
    expect(counter.current).toBeNull()
    expect(changes.at(-1)).toBeNull()
    // clear の後に届いた応答は捨てる
    counter.refresh('{}')
    const late = reqs[1]
    counter.clear()
    late.resolve({ total: 5 })
    await Promise.resolve()
    expect(counter.current).toBeNull()
  })
})
