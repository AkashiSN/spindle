// タグのキーの入力欄（D-113）。入力のたびに、ライブラリにあるキー（曲数の多い順）とよく使うキーから
// 前方一致の候補を出す。↑↓ で選び Enter / Tab かクリックで入れる。Esc で候補を閉じる。
// 候補を選んでいないときの Enter / Esc は呼び出し側の onKeyDown に渡す（追加の確定・取り消し）

import { useId, useState, type KeyboardEvent } from 'react'
import { useTagKeys } from '../hooks/useTagKeys'
import { filterTagKeys, moveActive } from '../lib/tagKeys'
import { formatCount } from '../lib/format'

export function TagKeyInput({
  value,
  onChange,
  onKeyDown,
  exclude,
  className,
  placeholder,
  ariaLabel,
  autoFocus,
  disabled,
}: {
  value: string
  onChange: (v: string) => void
  onKeyDown?: (e: KeyboardEvent<HTMLInputElement>) => void
  /** 既にある列・フィールド（候補から外す） */
  exclude?: readonly string[]
  className?: string
  placeholder?: string
  ariaLabel: string
  autoFocus?: boolean
  disabled?: boolean
}) {
  const { keys, load } = useTagKeys()
  const [open, setOpen] = useState(false)
  const [active, setActive] = useState(-1)
  const listId = useId()
  const hits = open ? filterTagKeys(keys, value, { exclude }) : []
  const pick = (key: string) => {
    onChange(key)
    setOpen(false)
    setActive(-1)
  }
  const handleKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (hits.length > 0 && !e.nativeEvent.isComposing) {
      if (e.key === 'ArrowDown' || e.key === 'ArrowUp') {
        e.preventDefault()
        const step = e.key === 'ArrowDown' ? 1 : -1
        setActive((a) => moveActive(a, hits.length, step))
        return
      }
      if ((e.key === 'Enter' || e.key === 'Tab') && active >= 0 && active < hits.length) {
        e.preventDefault()
        pick(hits[active].key)
        return
      }
      if (e.key === 'Escape') {
        e.preventDefault()
        e.stopPropagation()
        setOpen(false)
        return
      }
    }
    onKeyDown?.(e)
  }
  return (
    <span className="tagkey-input">
      <input
        type="text"
        className={className}
        value={value}
        placeholder={placeholder}
        aria-label={ariaLabel}
        autoFocus={autoFocus}
        disabled={disabled}
        role="combobox"
        aria-autocomplete="list"
        aria-expanded={hits.length > 0}
        aria-controls={listId}
        aria-activedescendant={active >= 0 && active < hits.length ? `${listId}-${active}` : undefined}
        autoComplete="off"
        spellCheck={false}
        onFocus={() => {
          load()
          setOpen(true)
        }}
        onBlur={() => setOpen(false)}
        onChange={(e) => {
          onChange(e.target.value)
          setOpen(true)
          setActive(-1)
        }}
        onKeyDown={handleKey}
      />
      {hits.length > 0 && (
        <ul className="tagkey-list" id={listId} role="listbox">
          {hits.map((k, i) => (
            <li
              key={k.key}
              id={`${listId}-${i}`}
              role="option"
              aria-selected={i === active}
              className={i === active ? 'active' : undefined}
              // blur より先に選ぶ（click だと入力欄の blur で候補が先に閉じる）
              onMouseDown={(e) => {
                e.preventDefault()
                pick(k.key)
              }}
            >
              <span>{k.key}</span>
              <span className="muted small">{k.tracks > 0 ? `${formatCount(k.tracks)} 曲` : 'よく使う'}</span>
            </li>
          ))}
        </ul>
      )}
    </span>
  )
}
