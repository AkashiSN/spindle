// 右カラム上部のプロパティ領域（SPEC §12.1 / §12.3、D-58）: タブ（プロパティ / 一括編集 / 操作）。
// 表の上に置き、高さはドラッグで可変（永続化）、折りたたみ可。
// プロパティは PropertiesPanel（詳細は hooks/useTrackDetails）、
// 一括編集の操作リスト・プレビュー・適用は BatchEditPanel（状態は hooks/useBatchEdit）、
// 操作（リネーム / 正規化 / RG / FLAC 検査 / プレイリストへ追加）は OperationsPanel（hooks/useOperations）

import type { Playlist, TrackDetail, TrackRow } from '../api/types'
import type { BatchEdit } from '../hooks/useBatchEdit'
import { useDragSize } from '../hooks/useDragSize'
import type { Operations } from '../hooks/useOperations'
import { formatCount } from '../lib/format'
import type { Selection } from '../lib/selection'
import { useLocalStorageState } from '../hooks/useLocalStorageState'
import { BatchEditPanel } from './BatchEditPanel'
import { OperationsPanel, type AlbumGainControl } from './OperationsPanel'
import { PropertiesPanel } from './PropertiesPanel'

export type SelectionSummary = {
  /** 選択件数。filter 形でサーバの件数がまだ無ければ null */
  count: number | null
  /** うち反映待ち。判定できない分は数えない */
  pending: number | null
}

export type PanelTab = 'props' | 'edit' | 'ops'

const MIN_H = 120
const MAX_H = 800

export function RightPanel({
  selection,
  summary,
  selectedRows,
  details,
  edit,
  ops,
  playlists,
  onAddToPlaylist,
  albumGain,
}: {
  selection: Selection
  summary: SelectionSummary
  /** 読み込み済みの選択行（詳細タブの共通値に使う。filter 形は表示中の一部だけ） */
  selectedRows: TrackRow[]
  /** 選択行の詳細（先頭 DETAIL_LIMIT 件。hooks/useTrackDetails） */
  details: { details: ReadonlyMap<number, TrackDetail>; loading: boolean; error: string | null }
  edit: BatchEdit
  ops: Operations
  /** 「プレイリストへ追加」の候補（手動のみ）と追加の実行（P1-6） */
  playlists: Playlist[]
  onAddToPlaylist: (playlistId: number) => void
  /** 選択行が属する album の album gain 切り替え（D-74） */
  albumGain: AlbumGainControl
}) {
  const [collapsed, setCollapsed] = useLocalStorageState<boolean>(
    'props.collapsed',
    false,
    (v): v is boolean => typeof v === 'boolean',
  )
  const { onMouseDown } = useDragSize({
    key: 'props.height',
    cssVar: '--props-h',
    fallback: 260,
    min: MIN_H,
    max: MAX_H,
    axis: 'y',
    direction: 1,
  })
  const [tab, setTab] = useLocalStorageState<PanelTab>(
    'panel.tab.v2',
    'props',
    (v): v is PanelTab => v === 'props' || v === 'edit' || v === 'ops',
  )

  if (collapsed) {
    return (
      <aside className="props-area collapsed">
        <button type="button" className="ghost" title="パネルを開く" onClick={() => setCollapsed(false)}>
          ▾ プロパティ
        </button>
      </aside>
    )
  }

  return (
    <aside className="props-area">
      <div className="panel-head">
        <div className="tabs" role="tablist">
          <button
            type="button"
            role="tab"
            aria-selected={tab === 'props'}
            className={tab === 'props' ? 'active' : ''}
            onClick={() => setTab('props')}
          >
            プロパティ
          </button>
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
            aria-selected={tab === 'ops'}
            className={tab === 'ops' ? 'active' : ''}
            onClick={() => setTab('ops')}
          >
            操作
          </button>
        </div>
        <span
          className="panel-count"
          title={
            selection.kind === 'filter'
              ? `フィルタ形の選択（選択時のフィルタ: ${selection.filter || '{}'}${selection.excludeIds.size > 0 ? `、除外 ${selection.excludeIds.size} 件` : ''}）`
              : undefined
          }
        >
          選択 {formatCount(summary.count)} 件
          {summary.pending != null && summary.pending > 0 && (
            <span className="muted">（うち反映待ち {formatCount(summary.pending)} 件）</span>
          )}
        </span>
        <button type="button" className="ghost" title="パネルを閉じる" onClick={() => setCollapsed(true)}>
          ▴
        </button>
      </div>
      {tab === 'edit' ? (
        <div className="panel-body">
          <BatchEditPanel edit={edit} hasSelection={selection.kind !== 'none'} />
        </div>
      ) : tab === 'ops' ? (
        <div className="panel-body">
          <OperationsPanel
            ops={ops}
            hasSelection={selection.kind !== 'none'}
            playlists={playlists}
            onAddToPlaylist={onAddToPlaylist}
            albumGain={albumGain}
          />
        </div>
      ) : (
        <div className="panel-body">
          <PropertiesPanel
            rows={selectedRows}
            selection={selection}
            details={details.details}
            loading={details.loading}
            error={details.error}
            onEdit={edit.applyToSelection}
            onDelete={edit.deleteFromSelection}
          />
        </div>
      )}
      <div className="divider-h" onMouseDown={onMouseDown} title="ドラッグで高さを変更" />
    </aside>
  )
}
