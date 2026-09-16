// filter 形の選択の「うち反映待ち」（SPEC §12.2、D-40）。
//
// 選択時のフィルタに `pending` flag を足して `GET /api/tracks?limit=1` の `total` を取る。
// 選択集合は immutable でも中の行の pending 状態はバッチの進行で変わるので、選択時だけでなく
// batch / library / resync / SSE 再接続のたびに取り直す。応答は世代で守り、古い応答が新しい
// 集計を上書きしない

import { DEFAULT_SORT, tracksUrl, type Filter } from './filter'

/** 選択時のフィルタ文字列（filterToParam の出力）に pending を足した一覧 URL */
export function pendingCountUrl(filterKey: string): string {
  let f: Filter = {}
  try {
    f = filterKey ? (JSON.parse(filterKey) as Filter) : {}
  } catch {
    f = {}
  }
  const flags = new Set(f.flags ?? [])
  flags.add('pending')
  return tracksUrl({ filter: { ...f, flags: [...flags] }, sort: DEFAULT_SORT, limit: 1 })
}

export type PendingFetch = (url: string, signal: AbortSignal) => Promise<{ total: number }>

export type PendingCount = { key: string; total: number }

export class PendingCounter {
  private generation = 0
  private abort: AbortController | null = null
  private value: PendingCount | null = null
  private readonly fetcher: PendingFetch
  private readonly onChange: (v: PendingCount | null) => void

  constructor(fetcher: PendingFetch, onChange: (v: PendingCount | null) => void) {
    this.fetcher = fetcher
    this.onChange = onChange
  }

  get current(): PendingCount | null {
    return this.value
  }

  /** `key` のフィルタで数え直す。進行中の取得は捨てる */
  refresh(key: string): void {
    this.generation += 1
    const gen = this.generation
    this.abort?.abort()
    const ctrl = new AbortController()
    this.abort = ctrl
    this.fetcher(pendingCountUrl(key), ctrl.signal)
      .then((p) => {
        if (gen !== this.generation) return
        this.value = { key, total: p.total }
        this.onChange(this.value)
      })
      .catch(() => {
        // 失敗・中断は前の値のまま（表示は「うち反映待ち」を出さないか古い値）
      })
  }

  /** 選択が filter 形でなくなった */
  clear(): void {
    this.generation += 1
    this.abort?.abort()
    this.abort = null
    if (this.value != null) {
      this.value = null
      this.onChange(null)
    }
  }
}
