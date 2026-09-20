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
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  // 絞り込みは要求の完了（成功 / 失敗）ごとに「どの filterParam に対する結果か」を持つ。
  // 現在の filterParam と一致する完了が無ければ応答待ち（失敗も完了なので待ちが残らない）
  const [filteredState, setFilteredState] = useState<{
    param: string
    items: AlbumRow[] | null
    error: string | null
  } | null>(null)
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
        if (seq === filteredSeq.current) setFilteredState({ param: filterParam, items: r.items, error: null })
      })
      .catch((e: unknown) => {
        if (seq === filteredSeq.current) {
          setFilteredState({ param: filterParam, items: null, error: e instanceof Error ? e.message : String(e) })
        }
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
  // 絞り込み無しなら全件。応答待ち・失敗・別のフィルタの結果しか無いときも全件（古い絞り込みを出さない）。
  // 同じフィルタの取り直し（library / job イベント）の間は前回の結果を出し、届いたら差し替える
  const current = filteredState?.param === filterParam ? filteredState : null
  const filterPending = filterParam !== '' && wantFiltered && current == null
  const filterError = current?.error ?? null
  const filtered = filterParam === '' || current?.items == null ? albums : current.items
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
  return { albums, filtered, filterPending, filterError, error, refresh, busy, setAlbumGain }
}
