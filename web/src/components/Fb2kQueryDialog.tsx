// foobar2000 Autoplaylist 用のクエリ表示（SPEC §10「foobar クエリへの変換」、P1-8、D-55）。
// Autoplaylist はファイルで渡せないので、クエリとソートパターンをコピーして foobar の
// 「Autoplaylist」ダイアログに貼ってもらう。変換できなかった指定は下に列挙する

import { useEffect, useState } from 'react'
import type { Fb2kQuery } from '../api/types'

export function Fb2kQueryDialog({ name, result, onClose }: { name: string; result: Fb2kQuery; onClose: () => void }) {
  const [copied, setCopied] = useState<'query' | 'sort' | null>(null)
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') onClose()
    }
    window.addEventListener('keydown', onKey)
    return () => window.removeEventListener('keydown', onKey)
  }, [onClose])
  const copy = async (which: 'query' | 'sort', text: string) => {
    try {
      await navigator.clipboard.writeText(text)
      setCopied(which)
    } catch {
      setCopied(null)
    }
  }
  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" role="dialog" aria-label="foobar2000 Autoplaylist クエリ" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <strong>foobar2000 Autoplaylist: {name}</strong>
          <button type="button" className="ghost" onClick={onClose} aria-label="閉じる">
            ×
          </button>
        </div>
        <label className="modal-field">
          <span>
            クエリ{' '}
            <button type="button" onClick={() => void copy('query', result.query)} disabled={result.query === ''}>
              {copied === 'query' ? 'コピーした' : 'コピー'}
            </button>
          </span>
          <textarea readOnly value={result.query} rows={3} placeholder="（変換できる条件なし）" />
        </label>
        <label className="modal-field">
          <span>
            ソートパターン{' '}
            <button type="button" onClick={() => void copy('sort', result.sort ?? '')} disabled={result.sort == null}>
              {copied === 'sort' ? 'コピーした' : 'コピー'}
            </button>
          </span>
          <input readOnly value={result.sort ?? ''} placeholder="（なし）" />
        </label>
        {result.notes.length > 0 && (
          <div className="modal-notes small">
            <div className="muted">変換できなかった指定:</div>
            <ul>
              {result.notes.map((n, i) => (
                <li key={i}>{n}</li>
              ))}
            </ul>
          </div>
        )}
      </div>
    </div>
  )
}
