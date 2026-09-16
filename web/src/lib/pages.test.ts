import { describe, expect, it } from 'vitest'
import { PageLoader, type Page, type PageFetcher } from './pages'

type Row = { id: number }

/** 1..n を size 件ずつ返す偽サーバ。cursor は次の先頭 id */
function fakeServer(n: number, size: number, log: string[] = []): PageFetcher<Row> {
  return async (cursor) => {
    const start = cursor ? Number(cursor) : 1
    log.push(`cursor=${cursor}`)
    await Promise.resolve()
    const items: Row[] = []
    for (let i = start; i < start + size && i <= n; i++) items.push({ id: i })
    const next = start + size <= n ? String(start + size) : null
    const page: Page<Row> = { items, next_cursor: next, total: n }
    return page
  }
}

async function settle(loader: PageLoader<Row>) {
  for (let i = 0; i < 50 && loader.getSnapshot().loading; i++) {
    await new Promise((r) => setTimeout(r, 0))
  }
}

describe('PageLoader', () => {
  it('要求された index まで順にページを積む', async () => {
    const log: string[] = []
    let changes = 0
    const loader = new PageLoader(fakeServer(25, 10, log), () => changes++)
    expect(loader.getSnapshot().total).toBeNull()
    loader.ensure(0)
    await settle(loader)
    expect(loader.getSnapshot().rows.length).toBe(10)
    expect(loader.getSnapshot().total).toBe(25)
    expect(loader.getSnapshot().exhausted).toBe(false)

    loader.ensure(24) // 3 ページ目まで必要
    loader.ensure(15) // 進行中の要求に畳まれる
    await settle(loader)
    expect(loader.getSnapshot().rows.map((r) => r.id)).toEqual(
      Array.from({ length: 25 }, (_, i) => i + 1),
    )
    expect(loader.getSnapshot().exhausted).toBe(true)
    expect(log).toEqual(['cursor=null', 'cursor=11', 'cursor=21'])
    // 読み切った後の ensure は何もしない
    loader.ensure(100)
    await settle(loader)
    expect(log.length).toBe(3)
    expect(changes).toBeGreaterThan(0)
  })

  it('reset は古い応答を捨てて新しいフィルタの行だけを見せる', async () => {
    const pending: Array<() => void> = []
    const slow: PageFetcher<Row> = () =>
      new Promise((resolve) => {
        pending.push(() => resolve({ items: [{ id: 999 }], next_cursor: null, total: 1 }))
      })
    const loader = new PageLoader(slow, () => {})
    loader.ensure(0)
    loader.reset(fakeServer(3, 10))
    loader.ensure(0)
    await settle(loader)
    for (const r of pending) r()
    await settle(loader)
    expect(loader.getSnapshot().rows.map((r) => r.id)).toEqual([1, 2, 3])
  })

  it('reload は表示に必要な件数まで取り直して一度に差し替え、後ろは捨てる', async () => {
    let n = 30
    const server: PageFetcher<Row> = (cursor, signal) => fakeServer(n, 10)(cursor, signal)
    const loader = new PageLoader(server, () => {})
    loader.ensure(29)
    await settle(loader)
    expect(loader.getSnapshot().rows.length).toBe(30)

    n = 31 // スキャンで 1 行増えた
    await loader.reload(15)
    const s = loader.getSnapshot()
    expect(s.rows.length).toBe(20) // 15 件以上になる最小のページ境界
    expect(s.total).toBe(31)
    expect(s.exhausted).toBe(false)
    loader.ensure(30)
    await settle(loader)
    expect(loader.getSnapshot().rows.length).toBe(31)
  })

  it('reload 中の ensure は差し替え後に新しい cursor から続き、旧 cursor の行を混ぜない', async () => {
    // 手動で応答を返す偽サーバ。行 id は世代で区別する（gen 1: 1..、gen 2: 1001..）
    type Req = { cursor: string | null; resolve: (p: Page<Row>) => void }
    const reqs: Req[] = []
    let gen = 1
    const server: PageFetcher<Row> = (cursor) =>
      new Promise((resolve) => reqs.push({ cursor, resolve }))
    const page = (g: number, start: number, size: number, n: number): Page<Row> => ({
      items: Array.from({ length: Math.min(size, n - start + 1) }, (_, i) => ({ id: g * 1000 + start + i })),
      next_cursor: start + size <= n ? String(start + size) : null,
      total: n,
    })
    const loader = new PageLoader(server, () => {})
    // 世代 1 を 2 ページ読む
    loader.ensure(15)
    reqs.shift()!.resolve(page(gen, 1, 10, 30))
    await settle(loader)
    reqs.shift()!.resolve(page(gen, 11, 10, 30))
    await settle(loader)
    expect(loader.getSnapshot().rows.map((r) => r.id)).toEqual(
      Array.from({ length: 20 }, (_, i) => 1001 + i),
    )

    // 無効化（世代 2）。先頭ページの応答を待たせている間にスクロールで 25 行目が要求される
    gen = 2
    const reloadDone = loader.reload(10)
    await Promise.resolve()
    expect(reqs.length).toBe(1)
    expect(reqs[0].cursor).toBeNull()
    loader.ensure(25)
    expect(reqs.length).toBe(1) // reload 中は新しい取得を始めない（旧 cursor=21 で取りに行かない）

    // 差し替え → 続きは新しい cursor（11）から
    reqs.shift()!.resolve(page(gen, 1, 10, 30))
    await reloadDone
    await Promise.resolve()
    expect(loader.getSnapshot().rows.map((r) => r.id)).toEqual(
      Array.from({ length: 10 }, (_, i) => 2001 + i),
    )
    expect(reqs.map((r) => r.cursor)).toEqual(['11'])
    reqs.shift()!.resolve(page(gen, 11, 10, 30))
    await settle(loader)
    expect(reqs.map((r) => r.cursor)).toEqual(['21'])
    reqs.shift()!.resolve(page(gen, 21, 10, 30))
    await settle(loader)
    const ids = loader.getSnapshot().rows.map((r) => r.id)
    expect(ids).toEqual(Array.from({ length: 30 }, (_, i) => 2001 + i))
    expect(loader.getSnapshot().exhausted).toBe(true)
  })

  it('ensure の取得中に reload が来たら古い応答を捨てて新しい行だけになる', async () => {
    type Req = { cursor: string | null; resolve: (p: Page<Row>) => void }
    const reqs: Req[] = []
    const server: PageFetcher<Row> = (cursor) =>
      new Promise((resolve) => reqs.push({ cursor, resolve }))
    const loader = new PageLoader(server, () => {})
    loader.ensure(0)
    const first = reqs.shift()!
    const reloadDone = loader.reload(1)
    await Promise.resolve()
    // 旧応答が後から届いても捨てられる
    first.resolve({ items: [{ id: 1 }], next_cursor: null, total: 1 })
    await Promise.resolve()
    reqs.shift()!.resolve({ items: [{ id: 2 }], next_cursor: null, total: 1 })
    await reloadDone
    expect(loader.getSnapshot().rows.map((r) => r.id)).toEqual([2])
    expect(loader.getSnapshot().loading).toBe(false)
  })

  it('dispose の後も ensure で読み直せる（StrictMode の effect 再実行）', async () => {
    const loader = new PageLoader(fakeServer(5, 10), () => {})
    loader.ensure(0)
    loader.dispose()
    expect(loader.getSnapshot().loading).toBe(false)
    loader.ensure(0)
    await settle(loader)
    expect(loader.getSnapshot().rows.length).toBe(5)
  })

  it('エラーは保持し、reset で消える', async () => {
    const failing: PageFetcher<Row> = async () => {
      throw new Error('HTTP 500')
    }
    const loader = new PageLoader(failing, () => {})
    loader.ensure(0)
    await settle(loader)
    expect(loader.getSnapshot().error).toBe('HTTP 500')
    loader.reset(fakeServer(1, 10))
    expect(loader.getSnapshot().error).toBeNull()
  })
})
