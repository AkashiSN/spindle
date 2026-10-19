// キーごとの世代番号（lib/latest.ts の Latest をキー単位にしたもの）。
// 同じ購読の進み具合を続けて取り直すと、A → B の順に出した要求が B → A の順に返ることがある。
// 応答時に isCurrent で「そのキーの最新の要求か」を見て、古い応答を捨てる

import { Latest } from '../lib/latest'

export class LatestByKey<K> {
  private gens = new Map<K, Latest>()

  /** key の新しい要求を始める。戻り値を応答時の isCurrent に渡す */
  next(key: K): number {
    let g = this.gens.get(key)
    if (!g) {
      g = new Latest()
      this.gens.set(key, g)
    }
    return g.next()
  }

  isCurrent(key: K, id: number): boolean {
    return this.gens.get(key)?.isCurrent(id) ?? false
  }

  /** すべてのキーの進行中の要求を無効にする（画面を閉じたとき） */
  invalidate(): void {
    for (const g of this.gens.values()) g.invalidate()
  }
}
