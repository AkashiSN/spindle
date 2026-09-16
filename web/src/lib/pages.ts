// カーソルページングを仮想スクロールに繋ぐローダ（D-40）。
//
// サーバの GET /api/tracks はキーセットなので、N ページ目を単独では取れない。行は先頭から
// 順に積み、仮想スクロールが未読込の index を要求したら足りるまで次ページを続けて取る。
// SSE library で無効化されたら **表示に必要な件数まで** 先頭から取り直し、揃ってから差し替える
// （1 ページずつ差し替えるとその間 rows が縮んでスクロール位置が飛ぶ）

export type Page<Row> = { items: Row[]; next_cursor: string | null; total: number }
export type PageFetcher<Row> = (cursor: string | null, signal: AbortSignal) => Promise<Page<Row>>

export type LoaderSnapshot<Row> = {
  rows: readonly Row[]
  /** フィルタに一致する全件数。1 ページ目が来るまで null */
  total: number | null
  loading: boolean
  error: string | null
  /** 最後まで読んだか */
  exhausted: boolean
}

export class PageLoader<Row> {
  private rows: Row[] = []
  private total: number | null = null
  private nextCursor: string | null | undefined = undefined
  private loading = false
  private error: string | null = null
  private generation = 0
  private abort: AbortController | null = null
  private snapshot: LoaderSnapshot<Row> | null = null
  /** 進行中の `ensure` が満たすべき index（複数の要求は最大値に畳む） */
  private wanted = -1
  private pump: Promise<void> | null = null
  /** reload の差し替え待ち。この間の ensure は wanted に畳み、差し替え後に 1 本の pump で続ける */
  private reloading = false

  private fetcher: PageFetcher<Row>
  private readonly onChange: () => void

  constructor(fetcher: PageFetcher<Row>, onChange: () => void) {
    this.fetcher = fetcher
    this.onChange = onChange
  }

  /** React 向けの不変スナップショット（同じ状態なら同じ参照） */
  getSnapshot(): LoaderSnapshot<Row> {
    if (!this.snapshot) {
      this.snapshot = {
        rows: this.rows,
        total: this.total,
        loading: this.loading,
        error: this.error,
        exhausted: this.nextCursor === null,
      }
    }
    return this.snapshot
  }

  private changed() {
    this.snapshot = null
    this.onChange()
  }

  /** フィルタ・ソートが変わった。積んだ行を捨てて最初から */
  reset(fetcher: PageFetcher<Row>) {
    this.fetcher = fetcher
    this.generation += 1
    this.abort?.abort()
    this.abort = null
    this.pump = null
    this.reloading = false
    this.rows = []
    this.total = null
    this.nextCursor = undefined
    this.loading = false
    this.error = null
    this.wanted = -1
    this.changed()
  }

  /**
   * `index` 行目が読めている状態にする（足りなければ順にページを取る）。
   * reload 中は要求を記録するだけで取得を始めない（旧 cursor からの続きが新しい行に混ざる）
   */
  ensure(index: number): void {
    if (index < this.rows.length || this.nextCursor === null || this.error) return
    this.wanted = Math.max(this.wanted, index)
    if (this.reloading) return
    if (!this.pump) this.pump = this.run()
  }

  private async run(): Promise<void> {
    const gen = this.generation
    const ctrl = new AbortController()
    this.abort = ctrl
    this.loading = true
    this.changed()
    try {
      while (this.rows.length <= this.wanted && this.nextCursor !== null) {
        const page = await this.fetcher(this.nextCursor ?? null, ctrl.signal)
        if (gen !== this.generation) return
        this.rows = this.rows.concat(page.items)
        this.total = page.total
        this.nextCursor = page.next_cursor
        this.changed()
      }
    } catch (e) {
      if (gen !== this.generation) return
      if (e instanceof DOMException && e.name === 'AbortError') return
      this.error = e instanceof Error ? e.message : String(e)
    } finally {
      if (gen === this.generation) {
        this.loading = false
        this.pump = null
        this.wanted = -1
        this.changed()
      }
    }
  }

  /**
   * 無効化。先頭から `keep` 件（表示に必要な範囲）を取り直し、揃ってから差し替える。
   * それより後ろは捨て、スクロールで再び要求されたら読む
   */
  async reload(keep: number): Promise<void> {
    this.generation += 1
    const gen = this.generation
    this.abort?.abort()
    const ctrl = new AbortController()
    this.abort = ctrl
    this.pump = null
    this.reloading = true
    // 差し替え後に続きが要るかは wanted で判断する（reload 中の ensure もここに畳まれる）
    this.wanted = Math.max(this.wanted, keep - 1)
    this.loading = true
    this.error = null
    this.changed()
    const fresh: Row[] = []
    let cursor: string | null | undefined = undefined
    let total: number | null = null
    try {
      while (fresh.length < Math.max(keep, 1) && cursor !== null) {
        const page: Page<Row> = await this.fetcher(cursor ?? null, ctrl.signal)
        if (gen !== this.generation) return
        fresh.push(...page.items)
        total = page.total
        cursor = page.next_cursor
      }
      this.rows = fresh
      this.total = total
      this.nextCursor = cursor
    } catch (e) {
      if (gen !== this.generation) return
      if (e instanceof DOMException && e.name === 'AbortError') return
      this.error = e instanceof Error ? e.message : String(e)
    } finally {
      if (gen === this.generation) {
        this.reloading = false
        this.loading = false
        this.changed()
        // reload 中に溜まった要求を新しい cursor から 1 本の pump で続ける
        if (!this.error && this.rows.length <= this.wanted && this.nextCursor !== null) {
          this.pump = this.run()
        } else {
          this.wanted = -1
        }
      }
    }
  }

  /** 進行中の取得を止める（React StrictMode の effect 再実行や unmount）。積んだ行は残す */
  dispose() {
    this.generation += 1
    this.abort?.abort()
    this.abort = null
    this.pump = null
    this.reloading = false
    this.wanted = -1
    if (this.loading) {
      this.loading = false
      this.changed()
    }
  }
}
