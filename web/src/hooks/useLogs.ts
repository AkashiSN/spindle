// 記録画面の「ログ」タブ（GET /api/logs。D-106）。種別と album で絞り、50 件ずつ取る。
// 取得結果は「どの絞り込みの結果か」と一緒に持ち、表示中の絞り込みと違う結果は無いものとして扱う
// （切り替え直後に前の行を出さない）

import { useCallback, useEffect, useRef, useState } from 'react'
import { apiFetch } from '../api/client'
import type { LogKind, LogRun } from '../lib/records'

type Page = { items: LogRun[]; next_cursor: string | null }
type Data = { kind: LogKind; albumId: number | null; items: LogRun[]; next: string | null }

export function useLogs(enabled: boolean, kind: LogKind, albumId: number | null) {
  const [data, setData] = useState<Data | null>(null)
  const [error, setError] = useState<string | null>(null)
  // 切り替え・取り直しの後に古い応答が遅れて届いても混ぜない
  const gen = useRef(0)
  // 取得中は「さらに読み込む」を受け付けない（同じ cursor の二重取得を防ぐ）
  const busy = useRef(false)

  const load = useCallback(
    (cursor: string | null) => {
      const g = gen.current
      busy.current = true
      const q = new URLSearchParams({ kind })
      if (albumId != null) q.set('album_id', String(albumId))
      if (cursor != null) q.set('cursor', cursor)
      apiFetch<Page>(`/api/logs?${q}`)
        .then((p) => {
          if (g !== gen.current) return
          setData((prev) => {
            const same = prev != null && prev.kind === kind && prev.albumId === albumId
            return {
              kind,
              albumId,
              items: cursor == null || !same ? p.items : [...prev.items, ...p.items],
              next: p.next_cursor,
            }
          })
          setError(null)
        })
        .catch((e: unknown) => {
          if (g === gen.current) setError(e instanceof Error ? e.message : String(e))
        })
        .finally(() => {
          if (g === gen.current) busy.current = false
        })
    },
    [kind, albumId],
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

  const current = data != null && data.kind === kind && data.albumId === albumId ? data : null
  const loadMore = useCallback(() => {
    if (current?.next != null && !busy.current) load(current.next)
  }, [load, current])
  return { items: current?.items ?? null, hasMore: current?.next != null, loadMore, refresh, error }
}
