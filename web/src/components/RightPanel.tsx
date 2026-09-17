// 右パネル（SPEC §12.1 / §12.3）: 2 タブ（一括編集 / 選択の詳細）。折りたたみ可、幅は永続化。
// 一括編集の操作リスト・プレビュー・適用は BatchEditPanel（状態は hooks/useBatchEdit）

import { useEffect, useRef, useState } from 'react'
import type { Playlist, TrackRow } from '../api/types'
import type { BatchEdit } from '../hooks/useBatchEdit'
import { formatCount } from '../lib/format'
import type { Selection } from '../lib/selection'
import { useLocalStorageState } from '../hooks/useLocalStorageState'
import { BatchEditPanel } from './BatchEditPanel'

export type SelectionSummary = {
  /** 選択件数。filter 形でサーバの件数がまだ無ければ null */
  count: number | null
  /** うち反映待ち。判定できない分は数えない */
  pending: number | null
}

export type PanelTab = 'edit' | 'detail'

const MIN_W = 220
const MAX_W = 640

export function RightPanel({
  selection,
  summary,
  selectedRows,
  edit,
  playlists,
  onAddToPlaylist,
}: {
  selection: Selection
  summary: SelectionSummary
  /** 読み込み済みの選択行（詳細タブの共通値に使う。filter 形は表示中の一部だけ） */
  selectedRows: TrackRow[]
  edit: BatchEdit
  /** 「プレイリストへ追加」の候補（手動のみ）と追加の実行（P1-6） */
  playlists: Playlist[]
  onAddToPlaylist: (playlistId: number) => void
}) {
  const [addTo, setAddTo] = useState<number | ''>('')
  const [collapsed, setCollapsed] = useLocalStorageState<boolean>(
    'panel.collapsed',
    false,
    (v): v is boolean => typeof v === 'boolean',
  )
  const [width, setWidth] = useLocalStorageState<number>(
    'panel.width',
    320,
    (v): v is number => typeof v === 'number' && v >= MIN_W && v <= MAX_W,
  )
  const [tab, setTab] = useLocalStorageState<PanelTab>(
    'panel.tab',
    'edit',
    (v): v is PanelTab => v === 'edit' || v === 'detail',
  )

  // 幅の変更は CSS 変数で親グリッドへ伝える
  useEffect(() => {
    document.documentElement.style.setProperty('--panel-w', collapsed ? '28px' : `${width}px`)
  }, [collapsed, width])

  const drag = useRef<{ startX: number; startW: number } | null>(null)
  const onDividerDown = (e: React.MouseEvent) => {
    e.preventDefault()
    drag.current = { startX: e.clientX, startW: width }
    const move = (ev: globalThis.MouseEvent) => {
      if (!drag.current) return
      const w = Math.min(MAX_W, Math.max(MIN_W, drag.current.startW - (ev.clientX - drag.current.startX)))
      setWidth(w)
    }
    const up = () => {
      drag.current = null
      window.removeEventListener('mousemove', move)
      window.removeEventListener('mouseup', up)
    }
    window.addEventListener('mousemove', move)
    window.addEventListener('mouseup', up)
  }

  if (collapsed) {
    return (
      <aside className="right-panel collapsed">
        <button type="button" className="ghost" title="パネルを開く" onClick={() => setCollapsed(false)}>
          ◂
        </button>
      </aside>
    )
  }

  return (
    <aside className="right-panel">
      <div className="divider" onMouseDown={onDividerDown} title="ドラッグで幅を変更" />
      <div className="panel-head">
        <div className="tabs" role="tablist">
          <button
            type="button"
            role="tab"
            aria-selected={tab === 'edit'}
            className={tab === 'edit' ? 'active' : ''}
            onClick={() => setTab('edit')}
          >
            一括編集
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={tab === 'detail'}
            className={tab === 'detail' ? 'active' : ''}
            onClick={() => setTab('detail')}
          >
            選択の詳細
          </button>
        </div>
        <button type="button" className="ghost" title="パネルを閉じる" onClick={() => setCollapsed(true)}>
          ▸
        </button>
      </div>
      <div className="panel-summary">
        選択 {formatCount(summary.count)} 件
        {summary.pending != null && summary.pending > 0 && (
          <span className="muted">（うち反映待ち {formatCount(summary.pending)} 件）</span>
        )}
        {selection.kind === 'filter' && (
          <div className="muted small">
            フィルタ形の選択（選択時のフィルタ: <code>{selection.filter || '{}'}</code>
            {selection.excludeIds.size > 0 ? `、除外 ${selection.excludeIds.size} 件` : ''}）
          </div>
        )}
        {selection.kind !== 'none' && playlists.length > 0 && (
          <div className="add-to-playlist">
            <select value={addTo} onChange={(e) => setAddTo(e.target.value === '' ? '' : Number(e.target.value))}>
              <option value="">プレイリストへ追加…</option>
              {playlists
                .filter((p) => p.kind === 'manual')
                .map((p) => (
                  <option key={p.id} value={p.id}>
                    {p.name}
                  </option>
                ))}
            </select>
            <button
              type="button"
              disabled={addTo === ''}
              onClick={() => {
                if (addTo !== '') onAddToPlaylist(addTo)
              }}
            >
              追加
            </button>
          </div>
        )}
      </div>
      {tab === 'edit' ? (
        <div className="panel-body">
          <BatchEditPanel edit={edit} hasSelection={selection.kind !== 'none'} />
        </div>
      ) : (
        <div className="panel-body">
          <SelectionDetail rows={selectedRows} selection={selection} />
        </div>
      )}
    </aside>
  )
}

const DETAIL_FIELDS: Array<[keyof TrackRow, string]> = [
  ['title', 'Title'],
  ['artist_display', 'Artist'],
  ['album', 'Album'],
  ['albumartist', 'AlbumArtist'],
  ['date', 'Date'],
  ['category', 'Category'],
  ['codec', 'Codec'],
]

/** 読み込み済みの選択行について、共通の値と差異のあるフィールドを出す */
function SelectionDetail({ rows, selection }: { rows: TrackRow[]; selection: Selection }) {
  if (selection.kind === 'none' || rows.length === 0) {
    return <p className="muted small">行を選ぶと共通タグと差異をここに出す</p>
  }
  return (
    <div>
      {selection.kind === 'filter' && (
        <p className="muted small">読み込み済みの {rows.length} 行から算出（集合全体ではない）</p>
      )}
      <table className="kv">
        <tbody>
          {DETAIL_FIELDS.map(([field, label]) => {
            const values = new Set(rows.map((r) => String(r[field] ?? '')))
            const common = values.size === 1
            return (
              <tr key={field} className={common ? '' : 'differs'}>
                <th>{label}</th>
                <td>{common ? [...values][0] || <span className="muted">（空）</span> : `差異あり（${values.size} 種）`}</td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}
