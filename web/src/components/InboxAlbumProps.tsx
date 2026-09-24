// Inbox の承認画面の「② アルバム情報」（D-86）。ライブラリのプロパティ（PropertiesPanel）と同じ操作の
// 2 列の表: 行をクリックで選び、ダブルクリック（Enter / F2）で入力欄になり、Enter で確定、Esc で取り消す。
// ↑↓ で行を移る。category は統制語彙の選択欄、album gain はダブルクリックで切り替わる。
//
// 「変更」の印はファイルのタグから作った提案（proposal）と違う行。下段の Info はファイルから（表示のみ）。

import { useRef, useState, type KeyboardEvent } from 'react'
import { editSession } from '../lib/editSession'
import { destinationLabel, type InboxDraft, type InboxItem } from '../lib/inbox'
import { CategoryField } from './CategoryField'

type Row = {
  key: 'albumartist' | 'album' | 'date' | 'category' | 'album_gain'
  label: string
  placeholder?: string
}

const ROWS: Row[] = [
  { key: 'albumartist', label: 'アルバムアーティスト' },
  { key: 'album', label: 'アルバム' },
  { key: 'date', label: '日付', placeholder: 'YYYY / YYYY-MM / YYYY-MM-DD（空なら書かない）' },
  { key: 'category', label: 'category（配置先）' },
  { key: 'album_gain', label: 'album gain' },
]

function display(d: Pick<InboxDraft, Row['key']>, key: Row['key']): string {
  switch (key) {
    case 'album_gain':
      return d.album_gain ? '計算する（アルバム通し再生用）' : '計算しない'
    case 'category':
      return d.category ?? '（未分類 → _Unsorted）'
    case 'date':
      return d.date ?? ''
    default:
      return d[key]
  }
}

export function InboxAlbumProps({
  item,
  draft,
  editable,
  onChange,
}: {
  item: InboxItem
  draft: InboxDraft
  editable: boolean
  onChange: (patch: Partial<InboxDraft>) => void
}) {
  const [sel, setSel] = useState<Row['key'] | null>(null)
  const [editing, setEditing] = useState<{ key: Row['key']; text: string } | null>(null)
  const tableRef = useRef<HTMLTableElement>(null)
  // 1 回の編集で確定 / 取り消しは 1 回だけ（Esc の後の blur で確定しない）
  const session = useRef(editSession())
  const refocus = () => tableRef.current?.focus()

  const start = (key: Row['key']) => {
    if (!editable) return
    setSel(key)
    if (key === 'album_gain') {
      onChange({ album_gain: !draft.album_gain })
      return
    }
    session.current.start()
    setEditing({ key, text: key === 'category' ? (draft.category ?? '') : display(draft, key) })
  }
  const commit = () => {
    if (editing == null) return
    const v = editing.text
    if (editing.key === 'date') onChange({ date: v.trim() === '' ? null : v.trim() })
    else if (editing.key === 'albumartist' || editing.key === 'album') onChange({ [editing.key]: v })
    setEditing(null)
    refocus()
  }
  const cancel = () => {
    setEditing(null)
    refocus()
  }
  const onKey = (e: KeyboardEvent) => {
    if (editing != null) return
    const i = sel == null ? -1 : ROWS.findIndex((r) => r.key === sel)
    if (e.key === 'ArrowDown') {
      e.preventDefault()
      setSel(ROWS[Math.min(ROWS.length - 1, i + 1)].key)
    } else if (e.key === 'ArrowUp') {
      e.preventDefault()
      setSel(ROWS[Math.max(0, i - 1)].key)
    } else if ((e.key === 'Enter' || e.key === 'F2') && sel != null) {
      e.preventDefault()
      start(sel)
    } else if (e.key === 'Escape') {
      setSel(null)
    }
  }
  // 「変更」の基準: ファイルのタグから作った提案。album gain の初期値は追記先の現在値（draftFrom と同じ）
  const baseline = { ...item.proposal, album_gain: item.destination?.album_gain ?? item.proposal.album_gain }
  const discs = new Set(draft.tracks.map((t) => t.disc_no)).size
  const dest = destinationLabel(item.destination)

  return (
    <table className="kv inbox-kv" ref={tableRef} tabIndex={0} onKeyDown={onKey} aria-label="アルバム情報">
      <tbody>
        <tr className="inbox-kv-sect">
          <td colSpan={2}>Metadata — 配置のときタグに書く{editable ? '（ダブルクリックで編集）' : ''}</td>
        </tr>
        {ROWS.map((r) => {
          const value = display(draft, r.key)
          const original = display(baseline, r.key)
          const changed = value !== original
          if (editing?.key === r.key) {
            return (
              <tr
                key={r.key}
                className="selected editing"
                onKeyDown={(e) => {
                  // category の選択欄は Esc でやめる（テキスト欄は自分で扱う）
                  if (e.key === 'Escape' && r.key === 'category') cancel()
                }}
              >
                <th>{r.label}</th>
                <td>
                  {r.key === 'category' ? (
                    <CategoryField
                      inline
                      value={draft.category}
                      onChange={(v) => {
                        onChange({ category: v })
                        setEditing(null)
                        refocus()
                      }}
                    />
                  ) : (
                    <input
                      type="text"
                      aria-label={r.label}
                      autoFocus
                      value={editing.text}
                      placeholder={r.placeholder}
                      onChange={(e) => setEditing({ key: r.key, text: e.target.value })}
                      onKeyDown={(e) => {
                        const a = session.current.key(e.key)
                        if (a != null) e.preventDefault()
                        if (a === 'commit') commit()
                        else if (a === 'cancel') cancel()
                      }}
                      onBlur={() => {
                        if (session.current.blur() === 'commit') commit()
                      }}
                    />
                  )}
                </td>
              </tr>
            )
          }
          return (
            <tr
              key={r.key}
              className={[sel === r.key ? 'selected' : '', changed ? 'edited' : ''].join(' ')}
              title={changed ? `ファイルの値: ${original === '' ? '（空）' : original}` : undefined}
              onClick={(e) => {
                if (e.detail >= 2) start(r.key)
                else setSel(r.key)
              }}
            >
              <th>{r.label}</th>
              <td>
                {value === '' ? <span className="muted">（空。書かない）</span> : value}
                {changed && <span className="inbox-changed">変更</span>}
              </td>
            </tr>
          )
        })}
        <tr className="inbox-kv-sect">
          <td colSpan={2}>Info — ファイルから（表示のみ）</td>
        </tr>
        <tr className="ro">
          <th>MusicBrainz リリース</th>
          <td>
            {draft.release_id != null && draft.release_id !== '' ? (
              <a href={`https://musicbrainz.org/release/${draft.release_id}`} target="_blank" rel="noopener noreferrer">
                {draft.release_id}
              </a>
            ) : (
              '（未選択）'
            )}
          </td>
        </tr>
        <tr className="ro">
          <th>ディスク / トラック</th>
          <td>
            {discs} 枚 · {draft.tracks.length} 曲
          </td>
        </tr>
        {dest != null && (
          <tr className="ro">
            <th>追記先</th>
            <td>{dest}</td>
          </tr>
        )}
      </tbody>
    </table>
  )
}
