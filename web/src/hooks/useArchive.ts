// 記録画面の「退避ファイル」タブ（GET /api/archive。D-106）。状態で絞って 100 件ずつ取り、
// 「さらに読み込む」で続きを足す。取得結果は「どのフィルタの結果か」と一緒に持ち、
// 表示中のフィルタと違う結果は無いものとして扱う（切り替え直後に前の行を出さない）

import { useCallback, useEffect, useRef, useState } from 'react'
import { apiFetch } from '../api/client'
import type { ArchiveCounts, ArchiveFilter } from '../lib/records'
import type { ArchivedEntry } from '../lib/settings'

type Page = { items: ArchivedEntry[]; next_cursor: number | null; counts: ArchiveCounts }
type Data = { filter: ArchiveFilter; items: ArchivedEntry[]; next: number | null }

export function useArchive(enabled: boolean, filter: ArchiveFilter) {
  const [data, setData] = useState<Data | null>(null)
  const [counts, setCounts] = useState<ArchiveCounts | null>(null)
  const [error, setError] = useState<string | null>(null)
  // 切り替え・取り直しの後に古い応答が遅れて届いても混ぜない
  const gen = useRef(0)
  // 取得中は「さらに読み込む」を受け付けない（同じ cursor の二重取得を防ぐ）
  const busy = useRef(false)

  const load = useCallback(
    (cursor: number | null) => {
      const g = gen.current
      busy.current = true
      const q = new URLSearchParams({ state: filter })
      if (cursor != null) q.set('cursor', String(cursor))
      apiFetch<Page>(`/api/archive?${q}`)
        .then((p) => {
          if (g !== gen.current) return
          setData((prev) => ({
            filter,
            items: cursor == null || prev == null || prev.filter !== filter ? p.items : [...prev.items, ...p.items],
            next: p.next_cursor,
          }))
          setCounts(p.counts)
          setError(null)
        })
        .catch((e: unknown) => {
          if (g === gen.current) setError(e instanceof Error ? e.message : String(e))
        })
        .finally(() => {
          if (g === gen.current) busy.current = false
        })
    },
    [filter],
  )
  const refresh = useCallback(() => {
    gen.current++
    setData(null)
    load(null)
  }, [load])
  useEffect(() => {
    if (!enabled) return
    gen.current++
    load(null)
  }, [enabled, load])

  const current = data != null && data.filter === filter ? data : null
  const loadMore = useCallback(() => {
    if (current?.next != null && !busy.current) load(current.next)
  }, [load, current])
  return { items: current?.items ?? null, counts, hasMore: current?.next != null, loadMore, refresh, error }
}
