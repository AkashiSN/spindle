// 上部ナビ（SPEC §12.1、D-58）: 画面切替・検索・ジョブ要約「実行中 N · 反映待ち M · 失敗 K」。
// 「一覧」がホーム。ジョブ要約は SSE で更新される

import { useEffect, useState } from 'react'
import type { JobSummary } from '../api/types'
import { formatCount } from '../lib/format'
import { VIEWS, type View } from '../lib/views'

export function TopNav({
  view,
  onView,
  query,
  onQuery,
  summary,
  connected,
  onLogout,
}: {
  view: View
  onView: (v: View) => void
  query: string
  onQuery: (q: string) => void
  summary: JobSummary | null
  connected: boolean
  onLogout: () => void
}) {
  // 入力は 250ms 遅らせてからフィルタに反映する（1 文字ごとに取り直さない）
  const [draft, setDraft] = useState(query)
  useEffect(() => {
    if (draft === query) return
    const t = window.setTimeout(() => onQuery(draft), 250)
    return () => window.clearTimeout(t)
  }, [draft, query, onQuery])

  return (
    <header className="top-nav">
      <span className="brand">spindle</span>
      <input
        type="search"
        className="search"
        placeholder="検索（3 文字以上で部分一致、未満は前後一致なし LIKE）"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') onQuery(draft)
          if (e.key === 'Escape') {
            setDraft('')
            onQuery('')
          }
        }}
      />
      <nav className="views">
        {VIEWS.map(([v, label]) => (
          <button
            key={v}
            type="button"
            className={v === view ? 'active' : ''}
            onClick={() => onView(v)}
          >
            {label}
          </button>
        ))}
      </nav>
      <button type="button" className="ghost jobs-summary" onClick={() => onView('jobs')} title="ジョブ画面へ">
        {summary ? (
          <>
            実行中 {formatCount(summary.running)} · 反映待ち {formatCount(summary.pending_ops)} · 失敗{' '}
            <span className={summary.failed > 0 ? 'error' : ''}>{formatCount(summary.failed)}</span>
            {summary.queued > 0 ? <span className="muted"> · 待ち {formatCount(summary.queued)}</span> : null}
          </>
        ) : (
          <span className="muted">ジョブ要約を取得中…</span>
        )}
        <span className={`dot ${connected ? 'on' : 'off'}`} title={connected ? 'SSE 接続中' : 'SSE 切断（再接続中）'} />
      </button>
      <button type="button" className="ghost" onClick={onLogout}>
        ログアウト
      </button>
    </header>
  )
}
