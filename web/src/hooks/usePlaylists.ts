import { useCallback, useEffect, useState } from 'react'
import { apiFetch, apiPatch, apiPost } from '../api/client'
import type {
  AppendResponse,
  ExportProfileName,
  ExportResponse,
  ImportCandidate,
  ImportResponse,
  Playlist,
  PlaylistList,
  RefreshResponse,
  RulePreview,
} from '../api/types'
import { toSelectionBody, type Selection } from '../lib/selection'

export type Playlists = ReturnType<typeof usePlaylists>

/**
 * サイドバーのプレイリスト一覧と操作（P1-6）。一覧は数十件なので全件を取り、操作のたびと
 * library イベントで取り直す（missing の件数が変わる）。項目の並びは表が
 * `filter.playlist_id` + `sort=position` で取るのでここには持たない
 */
export function usePlaylists(enabled: boolean) {
  const [items, setItems] = useState<Playlist[]>([])
  const [error, setError] = useState<string | null>(null)
  const refresh = useCallback(() => {
    apiFetch<PlaylistList>('/api/playlists')
      .then((r) => {
        setItems(r.items)
        setError(null)
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
  }, [])
  useEffect(() => {
    if (enabled) refresh()
  }, [enabled, refresh])

  /** rule があればスマートプレイリスト（P1-7） */
  const create = useCallback(
    async (name: string, rule?: string) => {
      const p = await apiPost<Playlist>('/api/playlists', rule ? { name, rule } : { name })
      refresh()
      return p
    },
    [refresh],
  )
  const setRule = useCallback(
    async (id: number, rule: string) => {
      const p = await apiPatch<Playlist>(`/api/playlists/${id}`, { rule })
      refresh()
      return p
    },
    [refresh],
  )
  const previewRule = useCallback(
    (rule: string, signal?: AbortSignal) =>
      apiFetch<RulePreview>('/api/playlists/preview', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ rule }),
        signal,
      }),
    [],
  )
  const refreshSmart = useCallback(
    async (id: number) => {
      const r = await apiPost<RefreshResponse>(`/api/playlists/${id}/refresh`, {})
      refresh()
      return r
    },
    [refresh],
  )
  const rename = useCallback(
    async (id: number, name: string) => {
      const p = await apiPatch<Playlist>(`/api/playlists/${id}`, { name })
      refresh()
      return p
    },
    [refresh],
  )
  const remove = useCallback(
    async (id: number) => {
      await apiFetch(`/api/playlists/${id}`, { method: 'DELETE' })
      refresh()
    },
    [refresh],
  )
  /** selection を末尾に追加（ids 形は id の並びのまま、filter 形は `sort` の順） */
  const addTracks = useCallback(
    async (id: number, selection: Selection | number[], sort?: string) => {
      const body = Array.isArray(selection) ? { ids: selection } : toSelectionBody(selection)
      if (!body) return null
      const r = await apiPost<AppendResponse>(`/api/playlists/${id}/items`, { selection: body, sort })
      refresh()
      return r
    },
    [refresh],
  )
  /** selection（filter 形はサーバで解決）か id 列を外す */
  const removeTracks = useCallback(
    async (id: number, selection: Selection | number[]) => {
      const body = Array.isArray(selection) ? { track_ids: selection } : { selection: toSelectionBody(selection) }
      if (!Array.isArray(selection) && !body.selection) return null
      const r = await apiFetch<{ removed: number }>(`/api/playlists/${id}/items`, {
        method: 'DELETE',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify(body),
      })
      refresh()
      return r
    },
    [refresh],
  )
  const moveTracks = useCallback(async (id: number, trackIds: number[], before: number | null) => {
    await apiPost(`/api/playlists/${id}/items/move`, { track_ids: trackIds, before })
  }, [])
  const exportTo = useCallback(
    async (id: number, profile: ExportProfileName) => {
      const r = await apiPost<ExportResponse>(`/api/playlists/${id}/export?profile=${profile}`, {})
      refresh()
      return r
    },
    [refresh],
  )
  const importCandidates = useCallback(async () => {
    const r = await apiFetch<{ items: ImportCandidate[] }>('/api/playlists/import')
    return r.items
  }, [])
  const importFile = useCallback(
    async (path: string, name?: string) => {
      const r = await apiPost<ImportResponse>('/api/playlists/import', { path, name })
      refresh()
      return r
    },
    [refresh],
  )

  return {
    items,
    error,
    refresh,
    create,
    setRule,
    previewRule,
    refreshSmart,
    rename,
    remove,
    addTracks,
    removeTracks,
    moveTracks,
    exportTo,
    importCandidates,
    importFile,
  }
}
