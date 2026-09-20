// プロパティタブ（foobar2000 の Selection Properties の再現。D-58）。
// Metadata（標準タグ + 任意タグ全部）/ Location / General を 1 つの表に区切り付きで出す。
// Metadata の値はダブルクリックで編集でき、Enter で選択全体への 1 op の一括編集になる
// （preview → apply。hooks/useBatchEdit.applyToSelection）。foobar2000 の Properties と同じく、
// 行末の × か右クリックで「フィールドを削除」（delete op）、表の下の「フィールドを追加」で新しいキーに
// set op（P4-3、D-72）。詳細は先頭 DETAIL_LIMIT 件だけ取る

import { useEffect, useMemo, useState } from 'react'
import type { TrackDetail, TrackRow } from '../api/types'
import {
  DETAIL_LIMIT,
  canDeleteRow,
  generalRows,
  locationRows,
  metadataRows,
  newFieldKeyProblem,
  splitValues,
  type PropRow,
  type PropValue,
} from '../lib/properties'
import type { Selection } from '../lib/selection'
import { toSelectionBody } from '../lib/selection'
import { normKey } from '../lib/tagops'

/** 編集中の状態: 値の編集（既存の行）か、フィールドの追加（新しいキー） */
type Editing = { mode: 'value'; key: string; text: string } | { mode: 'add'; key: string; text: string }

export function PropertiesPanel({
  rows,
  selection,
  details,
  loading,
  error,
  onEdit,
  onDelete,
}: {
  /** 読み込み済みの選択行 */
  rows: TrackRow[]
  selection: Selection
  details: ReadonlyMap<number, TrackDetail>
  loading: boolean
  error: string | null
  /** Metadata の編集（選択全体）。失敗の理由を返す */
  onEdit: (key: string, values: string[]) => Promise<string | null>
  /** フィールドの削除（選択全体への delete op）。失敗の理由を返す */
  onDelete: (key: string) => Promise<string | null>
}) {
  // 編集中の状態は選択（kind / ids / filter / exclude）に紐づける。読み込み済み行が同じでも
  // 選択の意味が変われば（ids → filter 形など）捨てる。Enter は選択全体に効くため
  const selectionKey = JSON.stringify(toSelectionBody(selection))
  const [editState, setEditState] = useState<{
    sel: string
    editing: Editing | null
    error: string | null
    busy: boolean
  }>({ sel: selectionKey, editing: null, error: null, busy: false })
  const current = editState.sel === selectionKey ? editState : null
  const editing = current?.editing ?? null
  const editError = current?.error ?? null
  const busy = current?.busy ?? false
  const patch = (p: Partial<{ editing: Editing | null; error: string | null; busy: boolean }>) =>
    setEditState((s) => {
      const base = s.sel === selectionKey ? s : { sel: selectionKey, editing: null, error: null, busy: false }
      return { ...base, ...p, sel: selectionKey }
    })
  const setEditing = (e: Editing | null) => patch({ editing: e })
  const setEditError = (error: string | null) => patch({ error })
  // 右クリックのメニュー（Metadata の行）。他の場所のクリック・Escape で閉じる
  const [menu, setMenu] = useState<{ row: PropRow; x: number; y: number } | null>(null)
  useEffect(() => {
    if (menu == null) return
    const close = () => setMenu(null)
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setMenu(null)
    }
    window.addEventListener('mousedown', close)
    window.addEventListener('keydown', onKey)
    return () => {
      window.removeEventListener('mousedown', close)
      window.removeEventListener('keydown', onKey)
    }
  }, [menu])

  const sampled = useMemo(() => {
    const out: TrackDetail[] = []
    for (const r of rows.slice(0, DETAIL_LIMIT)) {
      const d = details.get(r.id)
      if (d) out.push(d)
    }
    return out
  }, [rows, details])
  const metadata = useMemo(() => metadataRows(sampled), [sampled])
  const location = useMemo(() => locationRows(rows, details), [rows, details])
  const general = useMemo(() => generalRows(rows, details), [rows, details])

  if (selection.kind === 'none' || rows.length === 0) {
    return <p className="muted small">行を選ぶとタグとファイル情報をここに出す</p>
  }

  /** 結果を編集状態へ書き戻す。待っている間に選択が変わっていたら捨てる（新しい選択の編集欄に書かない） */
  const settle = (startedFor: string, err: string | null) =>
    setEditState((s) => {
      if (s.sel !== startedFor) return s
      return { ...s, busy: false, error: err, editing: err == null ? null : s.editing }
    })

  const commit = async () => {
    if (!editing || busy) return
    const startedFor = selectionKey
    if (editing.mode === 'add') {
      const problem = newFieldKeyProblem(
        editing.key,
        metadata.map((r) => r.key),
      )
      const values = splitValues(editing.text)
      const err = problem ?? (values.length === 0 ? '値を入力してください（空のフィールドは追加しない）' : null)
      if (err) {
        setEditError(err)
        return
      }
      patch({ busy: true })
      settle(startedFor, await onEdit(normKey(editing.key), values))
      return
    }
    patch({ busy: true })
    settle(startedFor, await onEdit(editing.key, splitValues(editing.text)))
  }

  const remove = async (row: PropRow) => {
    if (busy || !canDeleteRow(row)) return
    const startedFor = selectionKey
    patch({ busy: true, editing: null, error: null })
    settle(startedFor, await onDelete(row.key))
  }

  const startEdit = (row: PropRow) => {
    if (busy) return
    setEditError(null)
    setEditing({ mode: 'value', key: row.key, text: row.value.kind === 'text' ? row.value.text : '' })
  }

  const startAdd = () => {
    if (busy) return
    setEditError(null)
    setEditing({ mode: 'add', key: '', text: '' })
  }

  const cancel = () => {
    setEditing(null)
    setEditError(null)
  }

  const onKeyDown = (e: React.KeyboardEvent) => {
    if (e.key === 'Enter') {
      e.preventDefault()
      void commit()
    } else if (e.key === 'Escape') {
      cancel()
    }
  }

  const partial =
    selection.kind === 'filter' || rows.length > DETAIL_LIMIT
      ? `先頭 ${Math.min(rows.length, DETAIL_LIMIT)} 件のタグから判定（集合全体ではない）`
      : null

  return (
    <div className="props">
      {(partial || loading || error) && (
        <p className="muted small">
          {partial}
          {loading && ' 読み込み中…'}
          {error && ` ${error}`}
        </p>
      )}
      {editError && editing?.mode !== 'add' && <p className="error small">{editError}</p>}
      {/* foobar2000 の Properties と同じ 2 列: 左に Metadata、右に Location と General。狭ければ 1 列 */}
      <div className="props-cols">
        <table className="kv props-table">
          <tbody>
            <Section title="Metadata" />
            {metadata.map((row) =>
              editing?.mode === 'value' && editing.key === row.key ? (
                <tr key={row.key} className="editing">
                  <th>{row.label}</th>
                  <td colSpan={2}>
                    <input
                      autoFocus
                      value={editing.text}
                      disabled={busy}
                      placeholder={row.value.kind === 'multiple' ? '<複数の値>' : ''}
                      onChange={(e) => setEditing({ mode: 'value', key: row.key, text: e.target.value })}
                      onKeyDown={onKeyDown}
                      onBlur={() => {
                        if (!busy) setEditing(null)
                      }}
                    />
                  </td>
                </tr>
              ) : (
                <ValueRow
                  key={row.key}
                  row={row}
                  onDoubleClick={() => startEdit(row)}
                  onContextMenu={(e) => {
                    e.preventDefault()
                    setMenu({ row, x: e.clientX, y: e.clientY })
                  }}
                  title="ダブルクリックで編集。右クリックで削除 / 追加"
                  action={
                    <button
                      type="button"
                      className="props-delete"
                      title={`${row.label} を選択全体から削除`}
                      aria-label={`${row.label} を削除`}
                      disabled={busy || !canDeleteRow(row)}
                      onClick={() => void remove(row)}
                    >
                      ×
                    </button>
                  }
                />
              ),
            )}
            {editing?.mode === 'add' && editError && (
              <tr className="props-add-error">
                <td colSpan={3} className="error small">
                  {editError}
                </td>
              </tr>
            )}
            {editing?.mode === 'add' ? (
              <tr className="editing props-add">
                <th>
                  <input
                    autoFocus
                    className="props-add-key"
                    value={editing.key}
                    disabled={busy}
                    placeholder="キー（例: CATALOGNUMBER）"
                    aria-label="追加するフィールドのキー"
                    onChange={(e) => setEditing({ mode: 'add', key: e.target.value, text: editing.text })}
                    onKeyDown={onKeyDown}
                  />
                </th>
                <td colSpan={2}>
                  <input
                    value={editing.text}
                    disabled={busy}
                    placeholder="値（; で多値）"
                    aria-label="追加するフィールドの値"
                    onChange={(e) => setEditing({ mode: 'add', key: editing.key, text: e.target.value })}
                    onKeyDown={onKeyDown}
                  />
                </td>
              </tr>
            ) : (
              <tr className="props-add-row">
                <td colSpan={3}>
                  <button type="button" className="link small" disabled={busy} onClick={startAdd}>
                    + フィールドを追加
                  </button>
                </td>
              </tr>
            )}
          </tbody>
        </table>
        {menu != null && (
          <ul
            className="props-menu"
            role="menu"
            style={{ left: menu.x, top: menu.y }}
            onMouseDown={(e) => e.stopPropagation()}
          >
            <li>
              <button
                type="button"
                role="menuitem"
                disabled={busy || !canDeleteRow(menu.row)}
                onClick={() => {
                  const row = menu.row
                  setMenu(null)
                  void remove(row)
                }}
              >
                「{menu.row.label}」を削除
              </button>
            </li>
            <li>
              <button
                type="button"
                role="menuitem"
                disabled={busy}
                onClick={() => {
                  setMenu(null)
                  startAdd()
                }}
              >
                フィールドを追加…
              </button>
            </li>
          </ul>
        )}
        <table className="kv props-table">
          <tbody>
            <Section title="Location" />
            {location.map((row) => (
              <ValueRow key={row.key} row={row} />
            ))}
            <Section title="General" />
            {general.map((row) => (
              <ValueRow key={row.key} row={row} />
            ))}
          </tbody>
        </table>
      </div>
    </div>
  )
}

function Section({ title }: { title: string }) {
  return (
    <tr className="section">
      <th colSpan={3}>{title}</th>
    </tr>
  )
}

function ValueRow({
  row,
  onDoubleClick,
  onContextMenu,
  title,
  action,
}: {
  row: PropRow
  onDoubleClick?: () => void
  onContextMenu?: (e: React.MouseEvent) => void
  title?: string
  /** 行末の操作（Metadata の ×）。無ければ列も出さない */
  action?: React.ReactNode
}) {
  return (
    <tr
      className={row.value.kind === 'multiple' ? 'differs' : ''}
      onDoubleClick={onDoubleClick}
      onContextMenu={onContextMenu}
      title={title}
    >
      <th>{row.label}</th>
      <td colSpan={action == null ? 2 : 1}>
        <Value v={row.value} />
      </td>
      {action != null && <td className="props-action">{action}</td>}
    </tr>
  )
}

function Value({ v }: { v: PropValue }) {
  switch (v.kind) {
    case 'text':
      return <>{v.text}</>
    case 'multiple':
      return <span className="muted">{'<複数の値>'}</span>
    case 'empty':
      return null
  }
}
