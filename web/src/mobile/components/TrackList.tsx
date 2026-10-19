// 曲の一覧（仮想スクロール）。GET /api/tracks を next_cursor で継ぎ足し、末尾 50 行に近づいたら次を読む。
// SSE の library（と再接続）で先頭から取り直す。ライブラリ・フォルダ・プレイリストで共用する

import { useVirtualizer } from '@tanstack/react-virtual'
import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError } from '../../api/client'
import type { TrackRow } from '../../api/types'
import { TrailingDebounce } from '../../lib/debounce'
import { filterToParam, sortToParam, type Filter, type Sort } from '../../lib/filter'
import { formatCount } from '../../lib/format'
import { listTracks } from '../api'
import { nextPageNeeded } from '../library'
import { useEventSubscription } from '../useEvents'

const ROW_H = 60

type State = {
  items: TrackRow[]
  cursor: string | null
  total: number | null
  error: string | null
}

const EMPTY: State = { items: [], cursor: null, total: null, error: null }

function message(e: unknown): string {
  if (e instanceof ApiError) return `読み込みに失敗しました（${e.code}）`
  return '読み込みに失敗しました'
}

function trackTitle(r: TrackRow): string {
  if (r.title) return r.title
  return r.rel_path.split('/').pop() ?? r.rel_path
}

export function TrackList({ filter, sort, onOpen }: { filter: Filter; sort: Sort; onOpen: (row: TrackRow) => void }) {
  const key = `${filterToParam(filter)}|${sortToParam(sort)}`
  const [state, setState] = useState<State>(EMPTY)
  const [loading, setLoading] = useState(false)
  const args = useRef({ filter, sort })
  // 取り直すたびに増やし、古い応答を捨てる
  const gen = useRef(0)
  const busy = useRef(false)
  const parentRef = useRef<HTMLDivElement>(null)

  const reload = useCallback(() => {
    const g = ++gen.current
    busy.current = true
    setLoading(true)
    const { filter, sort } = args.current
    listTracks(filter, sort)
      .then((p) => {
        if (g === gen.current) setState({ items: p.items, cursor: p.next_cursor, total: p.total, error: null })
      })
      .catch((e: unknown) => {
        if (g === gen.current) setState((s) => ({ ...s, cursor: null, error: message(e) }))
      })
      .finally(() => {
        if (g === gen.current) {
          busy.current = false
          setLoading(false)
        }
      })
  }, [])

  const more = useCallback((cursor: string) => {
    if (busy.current) return
    const g = gen.current
    busy.current = true
    setLoading(true)
    const { filter, sort } = args.current
    listTracks(filter, sort, cursor)
      .then((p) => {
        if (g === gen.current)
          setState((s) => ({ ...s, items: [...s.items, ...p.items], cursor: p.next_cursor, total: p.total }))
      })
      .catch((e: unknown) => {
        if (g === gen.current) setState((s) => ({ ...s, cursor: null, error: message(e) }))
      })
      .finally(() => {
        if (g === gen.current) {
          busy.current = false
          setLoading(false)
        }
      })
  }, [])

  // 条件が変わったら空にして先頭から
  const [prevKey, setPrevKey] = useState<string | null>(null)
  if (prevKey !== key) {
    setPrevKey(key)
    setState(EMPTY)
  }
  // 呼び出し側が毎回新しい filter / sort を渡しても取り直さないよう、取り直しは key の変化で決める
  useEffect(() => {
    args.current = { filter, sort }
  })
  useEffect(() => {
    parentRef.current?.scrollTo({ top: 0 })
    reload()
  }, [key, reload])

  // 走査中は library が続けて届くので間引く
  const [refetch] = useState(() => new TrailingDebounce(() => reload(), 500, 3000))
  useEffect(() => () => refetch.cancel(), [refetch])
  useEventSubscription({
    onLibrary: () => refetch.trigger(),
    onOpen: (reconnect) => {
      if (reconnect) refetch.trigger()
    },
  })

  const virtualizer = useVirtualizer({
    count: state.items.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => ROW_H,
    overscan: 10,
  })
  const vItems = virtualizer.getVirtualItems()
  const lastIndex = vItems.length > 0 ? vItems[vItems.length - 1]!.index : -1

  useEffect(() => {
    if (state.cursor != null && nextPageNeeded(lastIndex, state.items.length, true)) more(state.cursor)
  }, [lastIndex, state.items.length, state.cursor, more, loading])

  return (
    <div className="m-tracklist">
      {state.total != null && <div className="m-count">{formatCount(state.total)} 曲</div>}
      <div className="m-scroll" ref={parentRef}>
      {state.items.length === 0 && !loading && state.error == null && state.total != null && (
        <p className="m-placeholder">該当する曲はありません</p>
      )}
      <div style={{ height: virtualizer.getTotalSize(), position: 'relative' }}>
        {vItems.map((vi) => {
          const r = state.items[vi.index]
          if (!r) return null
          const sub = [r.artist_display, r.album].filter((v): v is string => !!v).join(' · ')
          return (
            <button
              key={r.id}
              type="button"
              className={r.missing_since != null ? 'm-row m-track-row missing' : 'm-row m-track-row'}
              style={{ position: 'absolute', top: 0, left: 0, width: '100%', height: ROW_H, transform: `translateY(${vi.start}px)` }}
              onClick={() => onOpen(r)}
            >
              <span className="m-row-main">
                <span className="m-row-title">{trackTitle(r)}</span>
                <span className="m-row-sub">{sub}</span>
              </span>
              <span className="m-badge">{r.codec.toUpperCase()}</span>
            </button>
          )
        })}
      </div>
      {loading && <p className="m-placeholder">読み込み中…</p>}
      {state.error != null && (
        <div className="m-center-row">
          <p className="m-error">{state.error}</p>
          <button type="button" onClick={reload}>
            再読み込み
          </button>
        </div>
      )}
      </div>
    </div>
  )
}
