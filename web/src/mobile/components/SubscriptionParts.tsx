// 購読の一覧と詳細で共有する小さな部品（ジャケットと進捗バー）

import { artworkUrl } from '../../lib/artwork'
import type { SubscriptionProgress } from '../api'
import { downloadCounts } from '../subscriptions'

export function SubscriptionArt({ hash }: { hash: string | null }) {
  if (hash == null) return <span className="m-art m-art-empty" aria-hidden />
  return <img className="m-art" src={artworkUrl(hash, 256)} alt="" loading="lazy" decoding="async" />
}

/** 進捗バー（終わった曲 / 全曲。失敗も「終わった」側に数える） */
export function SyncProgressBar({ progress }: { progress: SubscriptionProgress }) {
  const c = downloadCounts(progress)
  const ratio = c.total > 0 ? (c.done + c.failed) / c.total : 0
  return (
    <div
      className="m-sub-bar"
      role="progressbar"
      aria-valuemin={0}
      aria-valuemax={c.total}
      aria-valuenow={c.done + c.failed}
    >
      <span className={c.total === 0 ? 'm-sub-bar-fill indeterminate' : 'm-sub-bar-fill'} style={{ width: `${Math.round(ratio * 100)}%` }} />
    </div>
  )
}
