// プロパティタブ（foobar2000 の Selection Properties の再現。D-58）。
// Metadata（標準タグ + 任意タグ全部）/ Location / General を 1 つの表に区切り付きで出す。
// Metadata の値はダブルクリックで編集でき、Enter で選択全体への 1 op の一括編集になる
// （preview → apply。hooks/useBatchEdit.applyToSelection）。詳細は先頭 DETAIL_LIMIT 件だけ取る

import { useMemo, useState } from 'react'
import type { TrackDetail, TrackRow } from '../api/types'
import {
  DETAIL_LIMIT,
  generalRows,
  locationRows,
  metadataRows,
  splitValues,
  type PropRow,
  type PropValue,
} from '../lib/properties'
import type { Selection } from '../lib/selection'
import { toSelectionBody } from '../lib/selection'

export function PropertiesPanel({
  rows,
  selection,
  details,
  loading,
  error,
  onEdit,
}: {
  /** 読み込み済みの選択行 */
  rows: TrackRow[]
  selection: Selection
  details: ReadonlyMap<number, TrackDetail>
  loading: boolean
  error: string | null
  /** Metadata の編集（選択全体）。失敗の理由を返す */
  onEdit: (key: string, values: string[]) => Promise<string | null>
}) {
  // 編集中の状態は選択（kind / ids / filter / exclude）に紐づける。読み込み済み行が同じでも
  // 選択の意味が変われば（ids → filter 形など）捨てる。Enter は選択全体に効くため
  const selectionKey = JSON.stringify(toSelectionBody(selection))
  const [editState, setEditState] = useState<{
    sel: string
    editing: { key: string; text: string } | null
    error: string | null
    busy: boolean
  }>({ sel: selectionKey, editing: null, error: null, busy: false })
  const current = editState.sel === selectionKey ? editState : null
  const editing = current?.editing ?? null
  const editError = current?.error ?? null
  const busy = current?.busy ?? false
  const patch = (p: Partial<{ editing: { key: string; text: string } | null; error: string | null; busy: boolean }>) =>
    setEditState((s) => {
      const base = s.sel === selectionKey ? s : { sel: selectionKey, editing: null, error: null, busy: false }
      return { ...base, ...p, sel: selectionKey }
    })
  const setEditing = (e: { key: string; text: string } | null) => patch({ editing: e })
  const setEditError = (error: string | null) => patch({ error })

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

  const commit = async () => {
    if (!editing || busy) return
    const startedFor = selectionKey
    patch({ busy: true })
    const err = await onEdit(editing.key, splitValues(editing.text))
    // 待っている間に選択が変わっていたら結果は捨てる（新しい選択の編集欄に書かない）
    setEditState((s) => {
      if (s.sel !== startedFor) return s
      return { ...s, busy: false, error: err, editing: err == null ? null : s.editing }
    })
  }

  const startEdit = (row: PropRow) => {
    if (busy) return
    setEditError(null)
    setEditing({ key: row.key, text: row.value.kind === 'text' ? row.value.text : '' })
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
      {editError && <p className="error small">{editError}</p>}
      {/* foobar2000 の Properties と同じ 2 列: 左に Metadata、右に Location と General。狭ければ 1 列 */}
      <div className="props-cols">
        <table className="kv props-table">
          <tbody>
            <Section title="Metadata" />
            {metadata.map((row) =>
              editing?.key === row.key ? (
                <tr key={row.key} className="editing">
                  <th>{row.label}</th>
                  <td>
                    <input
                      autoFocus
                      value={editing.text}
                      disabled={busy}
                      placeholder={row.value.kind === 'multiple' ? '<複数の値>' : ''}
                      onChange={(e) => setEditing({ key: row.key, text: e.target.value })}
                      onKeyDown={(e) => {
                        if (e.key === 'Enter') {
                          e.preventDefault()
                          void commit()
                        } else if (e.key === 'Escape') {
                          setEditing(null)
                          setEditError(null)
                        }
                      }}
                      onBlur={() => {
                        if (!busy) setEditing(null)
                      }}
                    />
                  </td>
                </tr>
              ) : (
                <ValueRow key={row.key} row={row} onDoubleClick={() => startEdit(row)} title="ダブルクリックで編集" />
              ),
            )}
          </tbody>
        </table>
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
      <th colSpan={2}>{title}</th>
    </tr>
  )
}

function ValueRow({ row, onDoubleClick, title }: { row: PropRow; onDoubleClick?: () => void; title?: string }) {
  return (
    <tr className={row.value.kind === 'multiple' ? 'differs' : ''} onDoubleClick={onDoubleClick} title={title}>
      <th>{row.label}</th>
      <td>
        <Value v={row.value} />
      </td>
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
