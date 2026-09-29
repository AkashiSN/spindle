// 末尾で 1 回だけ実行する間引き（trailing debounce）。呼ばれるたびに待ちを延ばし、静かになってから
// `waitMs` 後に 1 回実行する。ただし最初の呼び出しから `maxWaitMs` を超えては延ばさない
// （ジョブが途切れず完了し続けても、いつまでも取り直さないことは無い）

export class TrailingDebounce {
  private timer: ReturnType<typeof setTimeout> | null = null
  private first: number | null = null
  private readonly fn: () => void
  private readonly waitMs: number
  private readonly maxWaitMs: number
  private readonly now: () => number

  constructor(fn: () => void, waitMs: number, maxWaitMs: number, now: () => number = () => Date.now()) {
    this.fn = fn
    this.waitMs = waitMs
    this.maxWaitMs = maxWaitMs
    this.now = now
  }

  /** 実行を予約する（既に予約があれば待ちを延ばす） */
  trigger(): void {
    const t = this.now()
    if (this.first == null) this.first = t
    if (this.timer != null) clearTimeout(this.timer)
    const delay = Math.max(0, Math.min(this.waitMs, this.first + this.maxWaitMs - t))
    this.timer = setTimeout(() => this.flush(), delay)
  }

  /** 予約があってもなくても今すぐ実行する（予約は消す） */
  flush(): void {
    this.cancel()
    this.fn()
  }

  /** 予約を捨てる（実行しない） */
  cancel(): void {
    if (this.timer != null) clearTimeout(this.timer)
    this.timer = null
    this.first = null
  }
}
