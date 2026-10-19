// 購読の詳細（/m/subscriptions/{id}）。見出しに戻る・名前、カードと同じ同期ボタンと進み具合、
// last_result の内訳、直近の同期で投入した曲ごとの状態、取れなかった動画。
// 状態の取得と取り直しは SubscriptionsTab が受け持つ（ここは表示だけ）

import type { Subscription } from '../../api/types'
import type { SubscriptionProgress } from '../api'
import {
  cardSummary,
  itemLabel,
  resultBreakdown,
  sortItems,
  syncButton,
  syncedAgo,
  syncPhase,
  unavailableLine,
} from '../subscriptions'
import { SubscriptionArt, SyncProgressBar } from './SubscriptionParts'

export function SubscriptionDetail({
  subscription: s,
  progress,
  busy,
  error,
  now,
  artworkHash,
  onSync,
  onBack,
}: {
  subscription: Subscription
  progress: SubscriptionProgress | null
  busy: boolean
  error: string | null
  now: number
  artworkHash: string | null
  onSync: () => void
  onBack: () => void
}) {
  const phase = progress ? syncPhase(progress) : null
  const btn = syncButton(phase, busy)
  const r = s.last_result
  const items = progress ? sortItems(progress.items) : []
  const summaryClass = phase === 'failed' ? 'm-sub-summary m-error-text' : phase === 'done' ? 'm-sub-summary m-ok-text' : 'm-sub-summary'
  return (
    <div className="m-library">
      <div className="m-subhead m-pl-head">
        <button type="button" className="m-icon" aria-label="一覧へ戻る" onClick={onBack}>
          ‹
        </button>
        <span className="m-row-main m-pl-name">{s.album}</span>
      </div>
      <div className="m-scroll">
        <div className="m-sub-card">
          <div className="m-sub-top">
            <div className="m-row m-sub-open">
              <SubscriptionArt hash={artworkHash} />
              <span className="m-row-main">
                <span className="m-row-title">
                  {s.albumartist} / {s.album}
                </span>
                <span className="m-row-sub">最後の同期: {syncedAgo(s.last_synced_at, now)}</span>
              </span>
            </div>
            <button type="button" className="m-sub-sync" disabled={btn.disabled} onClick={onSync}>
              {btn.label}
            </button>
          </div>
          {phase === 'running' && progress && <SyncProgressBar progress={progress} />}
          {progress && <p className={summaryClass}>{cardSummary(s, progress)}</p>}
          {error != null && <p className="m-error m-sub-summary">{error}</p>}
          {r != null && r.state === 'done' && <p className="m-sub-summary m-row-sub">{resultBreakdown(r)}</p>}
        </div>
        {(items.length > 0 || (r?.unavailable.length ?? 0) > 0) && (
          <ul className="m-list">
            {items.map((j) => {
              const l = itemLabel(j)
              return (
                <li key={j.id} className={`m-sub-item m-sub-item-${j.state}`}>
                  <span className="m-sub-icon" aria-hidden>
                    {l.icon}
                  </span>
                  <span className="m-row-main m-row-title">{l.text}</span>
                  <span className="m-sub-trail">{l.trail}</span>
                </li>
              )
            })}
            {(r?.unavailable ?? []).map((u) => (
              <li key={`u-${u.position}-${u.id}`} className="m-sub-item m-sub-item-skipped">
                <span className="m-sub-icon" aria-hidden>
                  −
                </span>
                <span className="m-row-main m-row-sub">{unavailableLine(u)}</span>
              </li>
            ))}
          </ul>
        )}
        <p className="m-note m-sub-foot">画面を閉じても同期はサーバで続きます</p>
      </div>
    </div>
  )
}
