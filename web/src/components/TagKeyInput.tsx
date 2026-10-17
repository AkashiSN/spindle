// タグのキーの入力欄（D-113）。入力のたびに、ライブラリにあるキー（曲数の多い順）とよく使うキーから
// 前方一致の候補を出す。↑↓ で選び Enter / Tab かクリックで入れる。キー操作の規則は lib/tagKeys の tagKeyAction
// （IME の変換中は何もしない。候補を選んでいないときの Enter / Esc は呼び出し側の onKeyDown に渡す）

import { useId, useState, type KeyboardEvent } from 'react'
import { useTagKeys } from '../hooks/useTagKeys'
import { filterTagKeys, tagKeyAction } from '../lib/tagKeys'
import { formatCount } from '../lib/format'

export function TagKeyInput({
  value,
  onChange,
  onKeyDown,
  exclude,
  accept,
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
  /** その欄で足せるキーか（足せないキーは候補に出さない） */
  accept?: (key: string) => boolean
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
  const hits = open ? filterTagKeys(keys, value, { exclude, accept }) : []
  const close = () => {
    setOpen(false)
    setActive(-1)
  }
  const pick = (key: string) => {
    onChange(key)
    close()
  }
  const handleKey = (e: KeyboardEvent<HTMLInputElement>) => {
    // keyCode 229 は IME の処理中（isComposing が立たない環境の Enter もここで止める）
    const composing = e.nativeEvent.isComposing || e.keyCode === 229
    const a = tagKeyAction({ hits: hits.length, active, composing }, e.key)
    switch (a.kind) {
      case 'ignore':
        return
      case 'move':
        e.preventDefault()
        setActive(a.active)
        return
      case 'pick':
        if (a.preventDefault) e.preventDefault()
        pick(hits[a.index].key)
        return
      case 'close':
        e.preventDefault()
        e.stopPropagation()
        close()
        return
      case 'close-forward':
        close()
        onKeyDown?.(e)
        return
      case 'forward':
        onKeyDown?.(e)
    }
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
        onBlur={close}
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
