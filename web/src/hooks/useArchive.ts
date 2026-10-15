// 記録画面の「退避ファイル」タブ（GET /api/archive。D-106）。状態で絞って 100 件ずつ取り、
// 「さらに読み込む」で続きを足す。状態を切り替えたら最初から取り直す

import { useCallback, useEffect, useRef, useState } from 'react'
import { apiFetch } from '../api/client'
import type { ArchiveCounts, ArchiveFilter } from '../lib/records'
import type { ArchivedEntry } from '../lib/settings'

type Page = { items: ArchivedEntry[]; next_cursor: number | null; counts: ArchiveCounts }

export function useArchive(enabled: boolean, filter: ArchiveFilter) {
  const [items, setItems] = useState<ArchivedEntry[] | null>(null)
  const [counts, setCounts] = useState<ArchiveCounts | null>(null)
  const [next, setNext] = useState<number | null>(null)
  const [error, setError] = useState<string | null>(null)
  // 切り替えの直後に前の状態の応答が遅れて届いても混ぜない
  const gen = useRef(0)

  const load = useCallback(
    (cursor: number | null) => {
      const g = gen.current
      const q = new URLSearchParams({ state: filter })
      if (cursor != null) q.set('cursor', String(cursor))
      apiFetch<Page>(`/api/archive?${q}`)
        .then((p) => {
          if (g !== gen.current) return
          setItems((prev) => (cursor == null || prev == null ? p.items : [...prev, ...p.items]))
          setCounts(p.counts)
          setNext(p.next_cursor)
          setError(null)
        })
        .catch((e: unknown) => {
          if (g === gen.current) setError(e instanceof Error ? e.message : String(e))
        })
    },
    [filter],
  )
  const refresh = useCallback(() => {
    gen.current++
    setItems(null)
    load(null)
  }, [load])
  useEffect(() => {
    // 状態の切り替えでは前の一覧を残したまま取り直す（effect 内で setState しない）
    if (!enabled) return
    gen.current++
    load(null)
  }, [enabled, load])
  const loadMore = useCallback(() => {
    if (next != null) load(next)
  }, [load, next])
  return { items, counts, hasMore: next != null, loadMore, refresh, error }
}
