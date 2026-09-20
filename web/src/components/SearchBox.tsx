// 検索ボックス（SPEC §12.1、D-39 / D-40）。左カラムのツリーの下（foobar2000 と同じ場所）。
// 入力は 250ms 遅らせてから表の filter.q に反映する（1 文字ごとに取り直さない）。Esc でクリア

import { useEffect, useState } from 'react'

export function SearchBox({ query, onQuery }: { query: string; onQuery: (q: string) => void }) {
  const [draft, setDraft] = useState(query)
  useEffect(() => {
    if (draft === query) return
    const t = window.setTimeout(() => onQuery(draft), 250)
    return () => window.clearTimeout(t)
  }, [draft, query, onQuery])
  return (
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
  )
}
