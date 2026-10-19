// 検索欄。入力は手元ですぐ反映し、確定値（onChange）は 250ms 間引いて渡す

import { useEffect, useRef, useState } from 'react'
import { TrailingDebounce } from '../../lib/debounce'

const WAIT_MS = 250

export function SearchBar({
  value,
  onChange,
  placeholder,
}: {
  value: string
  onChange: (q: string) => void
  placeholder?: string
}) {
  const [text, setText] = useState(value)
  const latest = useRef({ text, onChange })
  useEffect(() => {
    latest.current = { text, onChange }
  })
  const debounce = useRef<TrailingDebounce | null>(null)
  useEffect(() => {
    const d = new TrailingDebounce(() => latest.current.onChange(latest.current.text), WAIT_MS, WAIT_MS * 4)
    debounce.current = d
    return () => {
      d.cancel()
      debounce.current = null
    }
  }, [])

  // 外から値が変わった（戻る・見方の切り替え）ら入力欄も合わせる
  const [prev, setPrev] = useState(value)
  if (prev !== value) {
    setPrev(value)
    if (value !== text.trim()) setText(value)
  }

  const change = (t: string) => {
    setText(t)
    latest.current = { text: t, onChange }
    debounce.current?.trigger()
  }

  return (
    <div className="m-search">
      <input
        type="search"
        inputMode="search"
        enterKeyHint="search"
        value={text}
        placeholder={placeholder ?? '検索'}
        aria-label="検索"
        onChange={(e) => change(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === 'Enter') debounce.current?.flush()
        }}
      />
      {text !== '' && (
        <button
          type="button"
          className="m-search-clear"
          aria-label="検索語を消す"
          onClick={() => {
            change('')
            debounce.current?.flush()
          }}
        >
          ×
        </button>
      )}
    </div>
  )
}
