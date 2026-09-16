// トラック一覧（SPEC §12.2）。TanStack Table（列の表示・順・幅）+ TanStack Virtual（行の仮想化）。
//
// 行データは先頭から順に積まれる（hooks/useTracks）。仮想化の count は `total` なので、
// 未読込の index は骨組みの行を描き、その index まで読むよう `ensure` を呼ぶ。
// TanStack Table に渡す data は **表示中の窓だけ**（6 万行の Row オブジェクトを作り直さない）。
// 選択はこのコンポーネントの外（lib/selection.ts）が持ち、ここは見え方とクリックの通知だけ

import { useVirtualizer } from '@tanstack/react-virtual'
import {
  columnOrderingFeature,
  columnResizingFeature,
  columnSizingFeature,
  columnVisibilityFeature,
  createColumnHelper,
  tableFeatures,
  useTable,
  type ColumnOrderState,
  type ColumnSizingState,
  type ColumnVisibilityState,
} from '@tanstack/react-table'
import { useCallback, useEffect, useMemo, useRef, useState, type MouseEvent } from 'react'
import type { TrackRow } from '../api/types'
import type { Sort, SortKey } from '../lib/filter'
import { formatDuration, formatTrackNo } from '../lib/format'
import type { ClickModifiers, Selection, VisibleOrder } from '../lib/selection'
import { isSelected } from '../lib/selection'
import { useLocalStorageState } from '../hooks/useLocalStorageState'
import { Badges } from './Badges'

const features = tableFeatures({
  columnVisibilityFeature,
  columnOrderingFeature,
  columnSizingFeature,
  columnResizingFeature,
})
const helper = createColumnHelper<typeof features, TrackRow>()

/** 列 id → サーバのソートキー（無い列はソート不可） */
const SORT_OF: Partial<Record<string, SortKey>> = {
  no: 'album',
  title: 'title',
  artist: 'artist',
  album: 'album_title',
  albumartist: 'albumartist',
  date: 'date',
  duration: 'duration',
  codec: 'codec',
  rel_path: 'rel_path',
}

const columns = helper.columns([
  helper.display({
    id: 'sel',
    header: '',
    size: 28,
    minSize: 28,
    maxSize: 28,
    enableResizing: false,
    cell: () => null, // 選択状態は行側で描く（TanStack の rowSelection は使わない）
  }),
  helper.accessor((r) => formatTrackNo(r.disc_no, r.track_no), {
    id: 'no',
    header: '#',
    size: 56,
    minSize: 40,
  }),
  helper.accessor('title', { id: 'title', header: 'Title', size: 280, minSize: 60 }),
  helper.accessor('artist_display', { id: 'artist', header: 'Artist', size: 200, minSize: 60 }),
  helper.accessor('album', { id: 'album', header: 'Album', size: 220, minSize: 60 }),
  helper.accessor('albumartist', { id: 'albumartist', header: 'AlbumArtist', size: 180, minSize: 60 }),
  helper.accessor('date', { id: 'date', header: 'Date', size: 64, minSize: 48 }),
  helper.accessor('category', { id: 'category', header: 'Category', size: 100, minSize: 48 }),
  helper.accessor((r) => formatDuration(r.duration_ms), {
    id: 'duration',
    header: '長さ',
    size: 64,
    minSize: 48,
  }),
  helper.accessor('codec', { id: 'codec', header: 'Codec', size: 56, minSize: 40 }),
  helper.display({
    id: 'badges',
    header: 'バッジ',
    size: 150,
    minSize: 80,
    cell: (ctx) => <Badges track={ctx.row.original} />,
  }),
  helper.accessor('rel_path', { id: 'rel_path', header: 'rel_path', size: 400, minSize: 80 }),
])

const DEFAULT_ORDER: ColumnOrderState = [
  'sel',
  'no',
  'title',
  'artist',
  'album',
  'albumartist',
  'date',
  'category',
  'duration',
  'codec',
  'badges',
  'rel_path',
]
const DEFAULT_VISIBILITY: ColumnVisibilityState = { rel_path: false }
const ROW_HEIGHT = 28
const EMPTY_ROWS: TrackRow[] = []

const isRecord = (v: unknown): v is Record<string, unknown> =>
  typeof v === 'object' && v != null && !Array.isArray(v)
const isVisibility = (v: unknown): v is ColumnVisibilityState =>
  isRecord(v) && Object.values(v).every((x) => typeof x === 'boolean')
const isSizing = (v: unknown): v is ColumnSizingState =>
  isRecord(v) && Object.values(v).every((x) => typeof x === 'number')
const isOrder = (v: unknown): v is ColumnOrderState =>
  Array.isArray(v) && v.every((x) => typeof x === 'string')

export type TrackTableProps = {
  rows: readonly TrackRow[]
  total: number | null
  loading: boolean
  error: string | null
  ensure: (index: number) => void
  sort: Sort
  onSort: (key: SortKey) => void
  selection: Selection
  /** filter 形の選択をハイライトしてよいか（表示フィルタが選択時と同じとき。D-40） */
  highlightFilterSelection: boolean
  onRowClick: (id: number, mods: ClickModifiers, order: VisibleOrder) => void
  onSelectAll: () => void
  onClearSelection: () => void
  /** 表示中の末尾 index（無効化時に取り直す件数の目安） */
  onRangeChange: (endIndex: number) => void
}

export function TrackTable(props: TrackTableProps) {
  const { rows, total, ensure, sort, onSort, selection, highlightFilterSelection } = props
  const [columnVisibility, setColumnVisibility] = useLocalStorageState<ColumnVisibilityState>(
    'columns.visibility',
    DEFAULT_VISIBILITY,
    isVisibility,
  )
  const [columnOrder, setColumnOrder] = useLocalStorageState<ColumnOrderState>(
    'columns.order',
    DEFAULT_ORDER,
    isOrder,
  )
  const [columnSizing, setColumnSizing] = useLocalStorageState<ColumnSizingState>(
    'columns.sizing',
    {},
    isSizing,
  )
  const [chooserOpen, setChooserOpen] = useState(false)

  const scrollRef = useRef<HTMLDivElement>(null)
  const count = total ?? rows.length
  const virtualizer = useVirtualizer({
    count,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 12,
  })
  const items = virtualizer.getVirtualItems()
  const first = items[0]?.index ?? 0
  const last = items[items.length - 1]?.index ?? -1

  // 表示窓に未読込の行があれば読む。onRangeChange は無効化時の取り直し件数に使う
  const onRangeChange = props.onRangeChange
  useEffect(() => {
    if (last >= rows.length) ensure(last)
    onRangeChange(last)
  }, [last, rows.length, ensure, onRangeChange])

  // TanStack Table には窓の行だけを渡す
  const windowRows = useMemo(() => {
    if (last < 0) return EMPTY_ROWS
    const end = Math.min(last + 1, rows.length)
    return first < end ? rows.slice(first, end) : EMPTY_ROWS
  }, [rows, first, last])

  const table = useTable({
    features,
    columns,
    data: windowRows,
    getRowId: (r) => String(r.id),
    state: { columnVisibility, columnOrder, columnSizing },
    onColumnVisibilityChange: setColumnVisibility,
    onColumnOrderChange: setColumnOrder,
    onColumnSizingChange: setColumnSizing,
    enableColumnResizing: true,
    columnResizeMode: 'onChange',
  })
  const tableRows = table.getRowModel().rows
  const headers = table.getHeaderGroups()[0]?.headers ?? []
  const totalWidth = table.getTotalSize()

  const order: VisibleOrder = useMemo(() => rows.map((r) => r.id), [rows])
  const handleRowClick = useCallback(
    (e: MouseEvent, id: number) => {
      e.preventDefault()
      props.onRowClick(id, { shift: e.shiftKey, ctrl: e.ctrlKey || e.metaKey }, order)
    },
    [order, props],
  )

  // キーボード: Ctrl+A（フィルタ形の全選択）、Esc（解除）
  const handleKey = useCallback(
    (e: React.KeyboardEvent) => {
      if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'a') {
        e.preventDefault()
        props.onSelectAll()
      } else if (e.key === 'Escape') {
        props.onClearSelection()
      }
    },
    [props],
  )

  // 列ヘッダの並べ替え（HTML5 DnD）
  const dragging = useRef<string | null>(null)
  const moveColumn = (from: string, to: string) => {
    if (from === to) return
    setColumnOrder((prev) => {
      const base = prev.length ? [...prev] : [...DEFAULT_ORDER]
      const fi = base.indexOf(from)
      const ti = base.indexOf(to)
      if (fi < 0 || ti < 0) return prev
      base.splice(fi, 1)
      base.splice(ti, 0, from)
      return base
    })
  }

  const showFilterHighlight = selection.kind !== 'filter' || highlightFilterSelection
  const sortable = (id: string) => SORT_OF[id]

  return (
    <div className="track-table" onKeyDown={handleKey} tabIndex={0}>
      <div className="table-toolbar">
        <span className="muted">
          表示 {total == null ? '…' : total.toLocaleString('ja-JP')} 件
          {props.loading ? '（読み込み中…）' : ''}
        </span>
        {props.error && <span className="error">読み込みに失敗: {props.error}</span>}
        <span className="spacer" />
        <button type="button" className="ghost" onClick={() => setChooserOpen((o) => !o)}>
          列
        </button>
        {chooserOpen && (
          <div className="column-chooser" role="menu">
            {table.getAllLeafColumns().map((col) =>
              col.id === 'sel' ? null : (
                <label key={col.id}>
                  <input
                    type="checkbox"
                    checked={col.getIsVisible()}
                    onChange={col.getToggleVisibilityHandler()}
                  />
                  {typeof col.columnDef.header === 'string' ? col.columnDef.header : col.id}
                </label>
              ),
            )}
            <button
              type="button"
              className="ghost"
              onClick={() => {
                setColumnVisibility(DEFAULT_VISIBILITY)
                setColumnOrder(DEFAULT_ORDER)
                setColumnSizing({})
              }}
            >
              既定に戻す
            </button>
          </div>
        )}
      </div>
      <div className="table-scroll" ref={scrollRef}>
        <div className="table-inner" style={{ width: totalWidth }}>
          <div className="thead" role="row">
            {headers.map((header) => {
              const id = header.column.id
              const key = sortable(id)
              const active = key != null && sort.key === key
              return (
                <div
                  key={header.id}
                  role="columnheader"
                  className={`th${key ? ' sortable' : ''}${active ? ' sorted' : ''}`}
                  style={{ width: header.getSize() }}
                  draggable={id !== 'sel'}
                  onDragStart={() => {
                    dragging.current = id
                  }}
                  onDragOver={(e) => e.preventDefault()}
                  onDrop={(e) => {
                    e.preventDefault()
                    if (dragging.current && id !== 'sel') moveColumn(dragging.current, id)
                    dragging.current = null
                  }}
                  onClick={() => {
                    if (key) onSort(key)
                  }}
                  title={key ? 'クリックでソート、ドラッグで並べ替え' : undefined}
                >
                  {header.isPlaceholder ? null : <table.FlexRender header={header} />}
                  {active && <span className="sort-mark">{sort.desc ? '▼' : '▲'}</span>}
                  {header.column.getCanResize() && (
                    <div
                      className="resizer"
                      onMouseDown={header.getResizeHandler()}
                      onTouchStart={header.getResizeHandler()}
                      onClick={(e) => e.stopPropagation()}
                    />
                  )}
                </div>
              )
            })}
          </div>
          <div className="tbody" style={{ height: virtualizer.getTotalSize() }}>
            {items.map((item) => {
              const track = rows[item.index]
              if (!track) {
                return (
                  <div
                    key={`skeleton-${item.index}`}
                    className="tr skeleton"
                    style={{ transform: `translateY(${item.start}px)`, height: ROW_HEIGHT }}
                  />
                )
              }
              const row = tableRows[item.index - first]
              const selected = showFilterHighlight && isSelected(selection, track.id)
              const cls = [
                'tr',
                selected ? 'selected' : '',
                track.pending_batch_id != null ? 'pending' : '',
                track.missing_since != null ? 'missing' : '',
              ]
                .filter(Boolean)
                .join(' ')
              return (
                <div
                  key={track.id}
                  role="row"
                  aria-selected={selected}
                  aria-disabled={track.pending_batch_id != null}
                  className={cls}
                  style={{ transform: `translateY(${item.start}px)`, height: ROW_HEIGHT }}
                  onClick={(e) => handleRowClick(e, track.id)}
                >
                  {row
                    ? row.getAllCells().map((cell) =>
                        cell.column.id === 'sel' ? (
                          <div key={cell.id} className="td td-sel" style={{ width: cell.column.getSize() }}>
                            <input
                              type="checkbox"
                              tabIndex={-1}
                              checked={selected}
                              onClick={(e) => {
                                e.stopPropagation()
                                props.onRowClick(track.id, { shift: e.shiftKey, ctrl: true }, order)
                              }}
                              onChange={() => {}}
                            />
                          </div>
                        ) : (
                          <div
                            key={cell.id}
                            className={`td td-${cell.column.id}`}
                            style={{ width: cell.column.getSize() }}
                            title={
                              typeof cell.getValue() === 'string' ? (cell.getValue() as string) : undefined
                            }
                          >
                            <table.FlexRender cell={cell} />
                          </div>
                        ),
                      )
                    : null}
                </div>
              )
            })}
          </div>
        </div>
      </div>
    </div>
  )
}
