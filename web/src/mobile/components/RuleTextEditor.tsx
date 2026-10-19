// スマートプレイリストのルール文の画面。PC と同じ DSL をそのまま書く等幅の欄と、入力補助のボタン
// （カーソル位置に入れる）、件数・誤りの表示。値に " を書くときは \" と書く

import { useRef } from 'react'
import { insertSnippet, SNIPPETS, type Snippet } from '../ruleBuilder'
import type { RulePreviewState } from '../useRulePreview'
import { PreviewStatus } from './SmartEditor'

export function RuleTextEditor({
  value,
  preview,
  onChange,
}: {
  value: string
  preview: RulePreviewState
  onChange: (v: string) => void
}) {
  const ref = useRef<HTMLTextAreaElement>(null)
  const insert = (s: Snippet) => {
    const el = ref.current
    const start = el?.selectionStart ?? value.length
    const end = el?.selectionEnd ?? value.length
    const r = insertSnippet(value, start, end, s)
    onChange(r.value)
    // 値を反映した後にカーソルを置き直す
    requestAnimationFrame(() => {
      const t = ref.current
      if (!t) return
      t.focus()
      t.setSelectionRange(r.caret, r.caret)
    })
  }
  return (
    <div className="m-ruletext">
      <textarea
        ref={ref}
        className="m-ruletext-area"
        aria-label="ルール文"
        rows={5}
        value={value}
        autoCapitalize="off"
        autoCorrect="off"
        spellCheck={false}
        placeholder="%albumartist% IS ヰ世界情緒 AND NOT %category% IS _Unsorted ORDER BY %date% DESC LIMIT 100"
        onChange={(e) => onChange(e.target.value)}
      />
      <div className="m-snippets" role="toolbar" aria-label="入力補助">
        {SNIPPETS.map((s) => (
          <button
            key={s.label}
            type="button"
            // ボタンを押してもテキスト欄のフォーカス（とカーソル位置）を奪わない
            onPointerDown={(e) => e.preventDefault()}
            onClick={() => insert(s)}
          >
            {s.label}
          </button>
        ))}
      </div>
      <PreviewStatus state={preview} />
    </div>
  )
}
