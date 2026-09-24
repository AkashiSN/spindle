// 配置先の category（統制語彙から選ぶ。無ければ _Unsorted。その場で語彙を足せる。D-67）。
//
// P4-20 追記で CD 画面から編集を外したので、いまの使い手は Inbox の承認画面だけ
// （CD で吸い出したものも Inbox を通るので、category はそこで選ぶ）。

import { useState } from 'react'
import { useCategories } from '../hooks/useCategories'

export function CategoryField({
  value,
  onChange,
  inline = false,
}: {
  value: string | null
  onChange: (v: string | null) => void
  /** 表のセルの中で使う（見出しを出さず、選択欄にフォーカスする。Inbox のアルバム情報。D-86） */
  inline?: boolean
}) {
  const cats = useCategories(true)
  const [adding, setAdding] = useState('')
  const add = async () => {
    const name = adding.trim()
    if (name === '') return
    const c = await cats.create(name)
    if (c != null) {
      onChange(c.name)
      setAdding('')
    }
  }
  return (
    <label className={inline ? 'cd-category-inline' : 'cd-field cd-field-category'}>
      {!inline && <span>category（配置先。未選択なら _Unsorted）</span>}
      <select
        value={value ?? ''}
        aria-label="category"
        autoFocus={inline}
        onChange={(e) => onChange(e.target.value === '' ? null : e.target.value)}
      >
        <option value="">（未分類 → _Unsorted）</option>
        {cats.items.map((c) => (
          <option key={c.id} value={c.name}>
            {c.name}
          </option>
        ))}
      </select>
      <span className="cd-category-add">
        <input
          type="text"
          aria-label="新しい category"
          placeholder="語彙に追加"
          value={adding}
          onChange={(e) => setAdding(e.target.value)}
        />
        <button type="button" disabled={adding.trim() === ''} onClick={add}>
          追加
        </button>
      </span>
      {cats.error != null && <span className="error">{cats.error}</span>}
    </label>
  )
}
