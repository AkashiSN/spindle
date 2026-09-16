// フィルタ + ソートに対する行の読み込み。PageLoader（lib/pages.ts）を React に繋ぐ

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { apiFetch } from '../api/client'
import type { TrackPage, TrackRow } from '../api/types'
import { filterToParam, sortToParam, tracksUrl, type Filter, type Sort } from '../lib/filter'
import { PageLoader, type LoaderSnapshot, type PageFetcher } from '../lib/pages'

function fetcherFor(filter: Filter, sort: Sort): PageFetcher<TrackRow> {
  return (cursor, signal) => apiFetch<TrackPage>(tracksUrl({ filter, sort, cursor }), { signal })
}

export type TracksHandle = {
  snapshot: LoaderSnapshot<TrackRow>
  /** `index` 行目まで読み込む（仮想スクロールが未読込の行を描くときに呼ぶ） */
  ensure: (index: number) => void
  /** 表示に必要な件数まで取り直す（SSE library / resync） */
  reload: (keep: number) => void
  /** 読み込み済み行の id → 行 */
  byId: ReadonlyMap<number, TrackRow>
}

export function useTracks(filter: Filter, sort: Sort, enabled: boolean): TracksHandle {
  const [, bump] = useState(0)
  const [loader] = useState(
    () => new PageLoader<TrackRow>(fetcherFor(filter, sort), () => bump((n) => n + 1)),
  )
  // フィルタ・ソートの識別は文字列で（オブジェクトの参照が変わっても集合が同じなら取り直さない）
  const key = `${sortToParam(sort)}|${filterToParam(filter)}`
  const lastKey = useRef<string | null>(null)
  useEffect(() => {
    if (lastKey.current === key) return
    lastKey.current = key
    loader.reset(fetcherFor(filter, sort))
    if (enabled) loader.ensure(0)
  }, [key, filter, sort, enabled, loader])
  useEffect(() => {
    if (enabled) loader.ensure(0)
  }, [enabled, loader])
  useEffect(() => () => loader.dispose(), [loader])

  const snapshot = loader.getSnapshot()
  const ensure = useCallback((index: number) => loader.ensure(index), [loader])
  const reload = useCallback(
    (keep: number) => {
      void loader.reload(keep)
    },
    [loader],
  )
  const byId = useMemo(() => {
    const m = new Map<number, TrackRow>()
    for (const r of snapshot.rows) m.set(r.id, r)
    return m
  }, [snapshot.rows])
  return { snapshot, ensure, reload, byId }
}
