import { useCallback, useEffect, useState } from 'react'
import { apiFetch, apiPatch } from '../api/client'
import type { AlbumRow } from '../api/types'

/** サイドバーのツリー用。全件（数千）を 1 回で取り、library イベントで取り直す */
export function useAlbums(enabled: boolean) {
  const [albums, setAlbums] = useState<AlbumRow[]>([])
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const refresh = useCallback(() => {
    apiFetch<{ items: AlbumRow[] }>('/api/albums')
      .then((r) => {
        setAlbums(r.items)
        setError(null)
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
  }, [])
  useEffect(() => {
    if (enabled) refresh()
  }, [enabled, refresh])
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
  return { albums, error, refresh, busy, setAlbumGain }
}
