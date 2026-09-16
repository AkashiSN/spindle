// 上部ナビ（SPEC §12.1）: 検索ボックスと画面切替。「一覧」がホーム

import { useEffect, useState } from 'react'
import { VIEWS, type View } from '../lib/views'

export function TopNav({
  view,
  onView,
  query,
  onQuery,
  onLogout,
}: {
  view: View
  onView: (v: View) => void
  query: string
  onQuery: (q: string) => void
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
      <button type="button" className="ghost" onClick={onLogout}>
        ログアウト
      </button>
    </header>
  )
}
