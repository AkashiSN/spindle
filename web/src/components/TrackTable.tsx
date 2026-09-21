// トラック一覧（SPEC §12.2）。TanStack Table（列の表示・順・幅）+ TanStack Virtual（行の仮想化）。
//
// 行データは先頭から順に積まれる（hooks/useTracks）。仮想化の count は `total` なので、
// 未読込の index は骨組みの行を描き、その index まで読むよう `ensure` を呼ぶ。
// TanStack Table に渡す data は **表示中の窓だけ**（6 万行の Row オブジェクトを作り直さない）。
// 選択はこのコンポーネントの外（lib/selection.ts）が持ち、ここは見え方とクリック・キーの通知だけ。
// キーボードのカーソル行（P4-17）だけはここが持つ（選択とは別。見た目と移動の起点にすぎない）

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
import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent, type MouseEvent } from 'react'
import type { TrackRow } from '../api/types'
import type { Sort, SortKey } from '../lib/filter'
import { formatArtistAlbum, formatDuration, formatTitleArtist, formatTrackNo } from '../lib/format'
import { cellDiff, COLUMN_TAG, formatValues, type PreviewState } from '../lib/preview'
import { dropTarget, parseDragIds, serializeDragIds, TRACK_DRAG_TYPE, type DropHalf } from '../lib/playlists'
import { asNavKey, moveCursor } from '../lib/keynav'
import type { ClickModifiers, Selection, VisibleOrder } from '../lib/selection'
import { isSelected } from '../lib/selection'
import { useLocalStorageState } from '../hooks/useLocalStorageState'
import { BadgeLegend } from './BadgeLegend'
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
  artist_album: 'albumartist',
  title_artist: 'title',
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
    size: 44,
    minSize: 44,
    maxSize: 44,
    enableResizing: false,
    cell: () => null, // 選択状態は行側で描く（TanStack の rowSelection は使わない）
  }),
  helper.accessor((r) => formatTrackNo(r.disc_no, r.track_no), {
    id: 'no',
    header: '#',
    size: 56,
    minSize: 40,
  }),
  // foobar2000 の既定列（D-58）: Artist/album = `%album artist% - %album%`、
  // Title / track artist = `%title%[ // %track artist%]`（アーティストがアルバムアーティストと違うときだけ）
  helper.accessor((r) => formatArtistAlbum(r), {
    id: 'artist_album',
    header: 'Artist/album',
    size: 260,
    minSize: 60,
  }),
  helper.accessor((r) => formatTitleArtist(r), {
    id: 'title_artist',
    header: 'Title / track artist',
    size: 320,
    minSize: 60,
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
  'artist_album',
  'no',
  'title_artist',
  'duration',
  'badges',
  'title',
  'artist',
  'album',
  'albumartist',
  'date',
  'category',
  'codec',
  'rel_path',
]
/** 既定は foobar2000 の Playlist View と同じ 5 列 + バッジ。他は列選択で出す */
const DEFAULT_VISIBILITY: ColumnVisibilityState = {
  title: false,
  artist: false,
  album: false,
  albumartist: false,
  date: false,
  category: false,
  codec: false,
  rel_path: false,
}
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
  /** Shift + 移動キー: anchor（無ければ移動前のカーソル行 `from`）からカーソル行までの範囲そのものに置き換える（P4-17） */
  onRangeSelect: (id: number, order: VisibleOrder, from: number | null) => void
  onSelectAll: () => void
  onClearSelection: () => void
  /** 表示中の末尾 index（無効化時に取り直す件数の目安） */
  onRangeChange: (endIndex: number) => void
  /** 有効なプレビュー。該当セルに 旧→新 を重ね、選択中で変更なしの行は薄く描く（SPEC §12.3） */
  preview: PreviewState | null
  /** セルのダブルクリック編集（1 件バッチ）。失敗の理由を返す */
  onInlineEdit: (id: number, columnId: string, value: string) => Promise<string | null>
  /** 行先頭の ▶（その行から再生。P1-9） */
  onPlay: (track: TrackRow) => void
  /** 再生中の行（強調） */
  playingId: number | null
  /** プレイリスト scope（P1-6）: ツールバーの「除外」と Delete キー、position 順なら行のドラッグで並べ替え */
  playlist: {
    name: string
    /** sort が position 昇順のとき true。行のドロップで並べ替える */
    reorderable: boolean
    onRemoveSelected: () => void
    onReorder: (trackIds: number[], before: number | null) => void
  } | null
}

/** インライン編集中のセル */
type Editing = { id: number; columnId: string; value: string; error: string | null }

/** 編集できる列の現在値（入力の初期値） */
function editableValue(track: TrackRow, columnId: string): string | null {
  switch (columnId) {
    case 'title':
      return track.title ?? ''
    case 'artist':
      return track.artist_display ?? ''
    case 'album':
      return track.album ?? ''
    case 'albumartist':
      return track.albumartist ?? ''
    case 'date':
      return track.date ?? ''
    case 'no':
      return formatTrackNo(track.disc_no, track.track_no)
    default:
      return null
  }
}

export function TrackTable(props: TrackTableProps) {
  const { rows, total, ensure, sort, onSort, selection, highlightFilterSelection, preview, onInlineEdit } = props
  const [editing, setEditing] = useState<Editing | null>(null)
  const commitEdit = useCallback(
    async (e: Editing) => {
      const err = await onInlineEdit(e.id, e.columnId, e.value)
      if (err) setEditing((cur) => (cur && cur.id === e.id ? { ...cur, error: err } : cur))
      else setEditing(null)
    },
    [onInlineEdit],
  )
  const onEditKey = (ev: KeyboardEvent<HTMLInputElement>, e: Editing) => {
    ev.stopPropagation()
    if (ev.key === 'Enter') void commitEdit(e)
    else if (ev.key === 'Escape') setEditing(null)
  }
  const [columnVisibility, setColumnVisibility] = useLocalStorageState<ColumnVisibilityState>(
    'columns.v2.visibility',
    DEFAULT_VISIBILITY,
    isVisibility,
  )
  const [columnOrder, setColumnOrder] = useLocalStorageState<ColumnOrderState>(
    'columns.v2.order',
    DEFAULT_ORDER,
    isOrder,
  )
  const [columnSizing, setColumnSizing] = useLocalStorageState<ColumnSizingState>(
    'columns.v2.sizing',
    {},
    isSizing,
  )
  const [chooserOpen, setChooserOpen] = useState(false)
  const [legendOpen, setLegendOpen] = useState(false)

  const scrollRef = useRef<HTMLDivElement>(null)
  const count = total ?? rows.length
  // tbody の前に sticky のヘッダ（1 行ぶん）がある。scrollMargin で行の座標をスクロール要素に合わせ、
  // scrollPaddingStart で scrollToIndex がヘッダの下に行を出す
  const virtualizer = useVirtualizer({
    count,
    getScrollElement: () => scrollRef.current,
    estimateSize: () => ROW_HEIGHT,
    overscan: 12,
    scrollMargin: ROW_HEIGHT,
    scrollPaddingStart: ROW_HEIGHT,
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
  const showFilterHighlight = selection.kind !== 'filter' || highlightFilterSelection
  // キーボードのカーソル行（id で持つ。ソート・フィルタで行が入れ替わっても別の行を指さない）
  const [cursorId, setCursorId] = useState<number | null>(null)
  const handleRowClick = useCallback(
    (e: MouseEvent, id: number) => {
      e.preventDefault()
      setCursorId(id)
      props.onRowClick(id, { shift: e.shiftKey, ctrl: e.ctrlKey || e.metaKey }, order)
    },
    [order, props],
  )

  // キーボード（SPEC §12.2「キーボード」）: Ctrl+A（フィルタ形の全選択）、Esc（解除）、Delete（プレイリスト
  // scope で選択を除外）、移動キー（P4-17: 素の移動 = その行だけ選択、Shift = anchor からの範囲、Ctrl = カーソルだけ）、
  // Space（カーソル行をトグル）
  const handleKey = useCallback(
    (e: React.KeyboardEvent) => {
      const ctrl = e.ctrlKey || e.metaKey
      if (ctrl && e.key.toLowerCase() === 'a') {
        e.preventDefault()
        props.onSelectAll()
        return
      }
      if (e.key === 'Escape') {
        props.onClearSelection()
        return
      }
      if (e.key === 'Delete' && props.playlist && selection.kind !== 'none') {
        e.preventDefault()
        props.playlist.onRemoveSelected()
        return
      }
      if (e.key === ' ') {
        if (cursorId == null) return
        e.preventDefault()
        props.onRowClick(cursorId, { shift: false, ctrl: true }, order)
        return
      }
      const nav = asNavKey(e.key)
      if (!nav) return
      e.preventDefault() // 既定のスクロールを止め、カーソルの移動で追随させる
      const el = scrollRef.current
      const pageRows = el ? Math.floor(el.clientHeight / ROW_HEIGHT) - 1 : 1
      const cur = cursorId == null ? -1 : order.indexOf(cursorId)
      const next = moveCursor(nav, cur < 0 ? null : cur, rows.length, pageRows)
      if (next == null) return
      const id = order[next]
      if (id == null) return
      setCursorId(id)
      virtualizer.scrollToIndex(next, { align: 'auto' })
      if (e.shiftKey) props.onRangeSelect(id, order, cursorId)
      else if (!ctrl) props.onRowClick(id, { shift: false, ctrl: false }, order)
    },
    [props, selection.kind, cursorId, order, rows.length, virtualizer],
  )

  // 行のドラッグ（P1-6）: 掴んだ行が選択に入っていれば選択中の行（表示順）、そうでなければその 1 行
  const dragIds = useCallback(
    (id: number): number[] => {
      if (selection.kind === 'ids' && selection.ids.has(id)) {
        const visible = order.filter((x) => selection.ids.has(x))
        const seen = new Set(visible)
        for (const x of selection.ids) if (!seen.has(x)) visible.push(x)
        return visible
      }
      if (selection.kind === 'filter' && showFilterHighlight && isSelected(selection, id)) {
        // filter 形は読み込み済みの行だけ（集合全体は右パネルの「プレイリストへ追加」で）
        return order.filter((x) => isSelected(selection, x))
      }
      return [id]
    },
    [selection, order, showFilterHighlight],
  )
  const [dropMark, setDropMark] = useState<{ id: number; half: DropHalf } | null>(null)
  const dragOverRow = (e: React.DragEvent, id: number) => {
    const pl = props.playlist
    if (!pl?.reorderable || !e.dataTransfer.types.includes(TRACK_DRAG_TYPE)) return
    e.preventDefault()
    e.dataTransfer.dropEffect = 'move'
    const rect = e.currentTarget.getBoundingClientRect()
    const half: DropHalf = e.clientY - rect.top < rect.height / 2 ? 'above' : 'below'
    setDropMark((cur) => (cur && cur.id === id && cur.half === half ? cur : { id, half }))
  }
  const dropOnRow = (e: React.DragEvent, id: number) => {
    const pl = props.playlist
    setDropMark(null)
    if (!pl?.reorderable || !e.dataTransfer.types.includes(TRACK_DRAG_TYPE)) return
    e.preventDefault()
    const rect = e.currentTarget.getBoundingClientRect()
    const half: DropHalf = e.clientY - rect.top < rect.height / 2 ? 'above' : 'below'
    const ids = parseDragIds(e.dataTransfer.getData(TRACK_DRAG_TYPE))
    if (!ids) return
    const target = dropTarget(order, ids, id, half)
    if (target) pl.onReorder(ids, target.before)
  }

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

  const sortable = (id: string) => SORT_OF[id]

  return (
    <div className="track-table" onKeyDown={handleKey} tabIndex={0}>
      <div className="table-toolbar">
        <span className="muted">
          表示 {total == null ? '…' : total.toLocaleString('ja-JP')} 件
          {props.loading ? '（読み込み中…）' : ''}
        </span>
        {props.error && <span className="error">読み込みに失敗: {props.error}</span>}
        {props.playlist && (
          <button
            type="button"
            className="ghost"
            disabled={selection.kind === 'none'}
            title="選択した行をこのプレイリストから外す（Delete）"
            onClick={props.playlist.onRemoveSelected}
          >
            「{props.playlist.name}」から除外
          </button>
        )}
        {props.playlist && !props.playlist.reorderable && (
          <span className="muted small">並べ替えは position 昇順のときだけ</span>
        )}
        <span className="spacer" />
        <button
          type="button"
          className="ghost"
          title="バッジの意味"
          onClick={() => {
            setLegendOpen((o) => !o)
            setChooserOpen(false)
          }}
        >
          凡例
        </button>
        {legendOpen && <BadgeLegend />}
        <button
          type="button"
          className="ghost"
          onClick={() => {
            setChooserOpen((o) => !o)
            setLegendOpen(false)
          }}
        >
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
                    style={{ transform: `translateY(${item.start - ROW_HEIGHT}px)`, height: ROW_HEIGHT }}
                  />
                )
              }
              const row = tableRows[item.index - first]
              const selected = showFilterHighlight && isSelected(selection, track.id)
              const previewChanged = preview?.changesById.has(track.id) ?? false
              // プレビュー中: 選択集合にあるのに変更が無い行は薄く（SPEC §12.3）。filter 形は表示
              // フィルタが選択時と同じときだけ集合の内外を判定できる（D-40）
              const previewUnchanged =
                preview != null && !previewChanged && showFilterHighlight && isSelected(selection, track.id)
              const cls = [
                'tr',
                selected ? 'selected' : '',
                cursorId === track.id ? 'cursor' : '',
                track.pending_batch_id != null ? 'pending' : '',
                track.missing_since != null ? 'missing' : '',
                previewUnchanged ? 'preview-unchanged' : '',
                dropMark?.id === track.id ? `drop-${dropMark.half}` : '',
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
                  style={{ transform: `translateY(${item.start - ROW_HEIGHT}px)`, height: ROW_HEIGHT }}
                  onClick={(e) => handleRowClick(e, track.id)}
                  draggable={editing == null}
                  onDragStart={(e) => {
                    e.dataTransfer.setData(TRACK_DRAG_TYPE, serializeDragIds(dragIds(track.id)))
                    e.dataTransfer.effectAllowed = 'copyMove'
                  }}
                  onDragOver={(e) => dragOverRow(e, track.id)}
                  onDragLeave={() => setDropMark((cur) => (cur?.id === track.id ? null : cur))}
                  onDrop={(e) => dropOnRow(e, track.id)}
                >
                  {row
                    ? row.getVisibleCells().map((cell) =>
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
                            <button
                              type="button"
                              className={`play-row${props.playingId === track.id ? ' on' : ''}`}
                              tabIndex={-1}
                              disabled={track.missing_since != null}
                              title="この行から再生"
                              onClick={(e) => {
                                e.stopPropagation()
                                props.onPlay(track)
                              }}
                            >
                              ▶
                            </button>
                          </div>
                        ) : editing && editing.id === track.id && editing.columnId === cell.column.id ? (
                          <div key={cell.id} className="td td-editing" style={{ width: cell.column.getSize() }}>
                            <input
                              autoFocus
                              value={editing.value}
                              aria-invalid={editing.error != null}
                              title={editing.error ?? 'Enter で適用、Esc で取り消し'}
                              onChange={(ev) => setEditing({ ...editing, value: ev.target.value, error: null })}
                              onKeyDown={(ev) => onEditKey(ev, editing)}
                              onBlur={() => setEditing(null)}
                              onClick={(ev) => ev.stopPropagation()}
                            />
                          </div>
                        ) : (
                          <div
                            key={cell.id}
                            className={`td td-${cell.column.id}${COLUMN_TAG[cell.column.id] ? ' editable' : ''}`}
                            style={{ width: cell.column.getSize() }}
                            title={
                              typeof cell.getValue() === 'string' ? (cell.getValue() as string) : undefined
                            }
                            onDoubleClick={(ev) => {
                              // 反映待ちの行は編集不可（SPEC §12.2）
                              if (track.pending_batch_id != null) return
                              const v = editableValue(track, cell.column.id)
                              if (v == null) return
                              ev.stopPropagation()
                              setEditing({ id: track.id, columnId: cell.column.id, value: v, error: null })
                            }}
                          >
                            {(() => {
                              const diff = cellDiff(preview, track.id, cell.column.id)
                              if (!diff) return <table.FlexRender cell={cell} />
                              return (
                                <span className="diff" title={`${formatValues(diff.old)} → ${formatValues(diff.new)}`}>
                                  <s>{formatValues(diff.old) || '（空）'}</s>
                                  <span className="arrow">→</span>
                                  <span className="new">{formatValues(diff.new) || '（空）'}</span>
                                </span>
                              )
                            })()}
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
