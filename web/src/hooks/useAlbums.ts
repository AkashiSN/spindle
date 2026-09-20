import { useCallback, useEffect, useRef, useState } from 'react'
import { apiFetch, apiPatch } from '../api/client'
import type { AlbumRow } from '../api/types'
import { albumsUrl } from '../lib/artwork'

/**
 * サイドバーのツリー用の全件（数千。1 回で取り、library イベントで取り直す）と、アルバム画面用の
 * 絞り込み（`GET /api/albums?filter=`。表と同じフィルタ。P4-6）。絞り込みは `filterParam` が空でなく
 * `wantFiltered`（アルバム画面を表示中）のときだけ取り、空なら全件をそのまま使う
 */
export function useAlbums(enabled: boolean, filterParam = '', wantFiltered = false) {
  const [albums, setAlbums] = useState<AlbumRow[]>([])
  const [filteredState, setFilteredState] = useState<{ param: string; items: AlbumRow[] } | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  // 取り直しの最新だけを採用する（古い応答で新しい絞り込みを上書きしない）
  const filteredSeq = useRef(0)
  const refreshAll = useCallback(() => {
    apiFetch<{ items: AlbumRow[] }>(albumsUrl(''))
      .then((r) => {
        setAlbums(r.items)
        setError(null)
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
  }, [])
  const refreshFiltered = useCallback(() => {
    if (!wantFiltered || filterParam === '') return
    const seq = ++filteredSeq.current
    apiFetch<{ items: AlbumRow[] }>(albumsUrl(filterParam))
      .then((r) => {
        if (seq !== filteredSeq.current) return
        setFilteredState({ param: filterParam, items: r.items })
        setError(null)
      })
      .catch((e: unknown) => {
        if (seq === filteredSeq.current) setError(e instanceof Error ? e.message : String(e))
      })
  }, [filterParam, wantFiltered])
  const refresh = useCallback(() => {
    refreshAll()
    refreshFiltered()
  }, [refreshAll, refreshFiltered])
  useEffect(() => {
    if (enabled) refreshAll()
  }, [enabled, refreshAll])
  useEffect(() => {
    if (enabled) refreshFiltered()
  }, [enabled, refreshFiltered])
  // 絞り込み無しなら全件、絞り込み中で応答待ちなら前回の（別のフィルタの）結果を出さず全件
  const filtered = filterParam === '' ? albums : filteredState?.param === filterParam ? filteredState.items : albums
  const filterPending = filterParam !== '' && wantFiltered && filteredState?.param !== filterParam
  /** album gain の属性を切り替える（PATCH /api/albums/:id。D-74）。失敗ならメッセージ */
  const setAlbumGain = useCallback(
    async (id: number, on: boolean): Promise<string | null> => {
      setBusy(true)
      try {
        const r = await apiPatch<{ album: AlbumRow }>(`/api/albums/${id}`, { album_gain: on })
        setAlbums((list) => list.map((a) => (a.id === id ? r.album : a)))
        return null
      } catch (e: unknown) {
        return e instanceof Error ? e.message : String(e)
      } finally {
        setBusy(false)
      }
    },
    [],
  )
  return { albums, filtered, filterPending, error, refresh, busy, setAlbumGain }
}
