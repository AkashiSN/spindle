import { useCallback, useEffect, useState } from 'react'
import { apiFetch } from '../api/client'
import type { AlbumRow } from '../api/types'

/** サイドバーのツリー用。全件（数千）を 1 回で取り、library イベントで取り直す */
export function useAlbums(enabled: boolean) {
  const [albums, setAlbums] = useState<AlbumRow[]>([])
  const [error, setError] = useState<string | null>(null)
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
  return { albums, error, refresh }
}
