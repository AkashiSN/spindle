// 右パネル（SPEC §12.1 / §12.3）: 2 タブ（一括編集 / 選択の詳細）。折りたたみ可、幅は永続化。
// 一括編集の操作リストとプレビューは P0-10。ここでは選択の要約と骨格だけ

import { useEffect, useRef } from 'react'
import type { TrackRow } from '../api/types'
import { formatCount } from '../lib/format'
import type { Selection } from '../lib/selection'
import { useLocalStorageState } from '../hooks/useLocalStorageState'

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
}: {
  selection: Selection
  summary: SelectionSummary
  /** 読み込み済みの選択行（詳細タブの共通値に使う。filter 形は表示中の一部だけ） */
  selectedRows: TrackRow[]
}) {
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
      </div>
      {tab === 'edit' ? (
        <div className="panel-body">
          <p className="muted small">
            操作リスト（固定値 / フィールド参照 / 正規表現置換 / 連番 / 削除）とプレビュー・適用は
            P0-10 で載せる。選択は表示フィルタやソートを変えても変わらない。
          </p>
          <button type="button" disabled>
            プレビュー
          </button>{' '}
          <button type="button" disabled>
            適用
          </button>
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
