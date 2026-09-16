import { useCallback, useEffect, useRef, useState } from 'react'
import { apiFetch, ApiError, apiPost } from '../api/client'
import type { HistoryDetail, HistoryItem, HistoryList, RevertResponse } from '../api/types'

export type HistoryState = {
  items: HistoryItem[] | null
  error: string | null
  /** 開いている行の詳細（id → detail）。取得中は undefined */
  details: Record<number, HistoryDetail | undefined>
  /** 直近の操作の結果メッセージ（巻き戻し / キャンセルの成否） */
  notice: string | null
  refresh: () => void
  open: (id: number) => void
  close: (id: number) => void
  revert: (id: number) => Promise<void>
  cancel: (id: number) => Promise<void>
}

/**
 * 編集履歴画面（SPEC §12.4）。一覧は画面表示時と SSE batch のたびに取り直す（250ms で間引く）。
 * 開いている行の詳細も一覧と一緒に取り直す（op の result が動く）
 */
export function useHistory(enabled: boolean): HistoryState {
  const [items, setItems] = useState<HistoryItem[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [details, setDetails] = useState<Record<number, HistoryDetail | undefined>>({})
  const [notice, setNotice] = useState<string | null>(null)
  const openIds = useRef<Set<number>>(new Set())
  const timer = useRef<number | null>(null)

  const fetchDetail = useCallback((id: number) => {
    apiFetch<HistoryDetail>(`/api/history/${id}`)
      .then((d) => setDetails((prev) => ({ ...prev, [id]: d })))
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
  }, [])

  const fetchNow = useCallback(() => {
    apiFetch<HistoryList>('/api/history')
      .then((l) => {
        setItems(l.items)
        setError(null)
        for (const id of openIds.current) fetchDetail(id)
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
  }, [fetchDetail])

  const refresh = useCallback(() => {
    if (timer.current != null) return
    timer.current = window.setTimeout(() => {
      timer.current = null
      fetchNow()
    }, 250)
  }, [fetchNow])

  useEffect(() => {
    if (enabled) fetchNow()
  }, [enabled, fetchNow])

  const open = useCallback(
    (id: number) => {
      openIds.current.add(id)
      fetchDetail(id)
    },
    [fetchDetail],
  )
  const close = useCallback((id: number) => {
    openIds.current.delete(id)
    setDetails((prev) => {
      const next = { ...prev }
      delete next[id]
      return next
    })
  }, [])

  const revert = useCallback(
    async (id: number) => {
      try {
        const r = await apiPost<RevertResponse>(`/api/history/${id}/revert`, {})
        setNotice(
          `#${id} を #${r.batch_id} として巻き戻し中（${r.affected} 件${r.conflict > 0 ? `、うち conflict ${r.conflict} 件` : ''}）`,
        )
      } catch (e) {
        setNotice(revertErrorMessage(id, e))
      }
      fetchNow()
    },
    [fetchNow],
  )

  const cancel = useCallback(
    async (id: number) => {
      try {
        await apiPost<void>(`/api/history/${id}/cancel`, {})
        setNotice(`#${id} をキャンセルした`)
      } catch (e) {
        setNotice(e instanceof ApiError && e.code === 'not_cancellable' ? `#${id} は既に終端状態` : String(e))
      }
      fetchNow()
    },
    [fetchNow],
  )

  return { items, error, details, notice, refresh, open, close, revert, cancel }
}

export function revertErrorMessage(id: number, e: unknown): string {
  if (e instanceof ApiError) {
    switch (e.code) {
      case 'not_terminal':
        return `#${id} は反映中。先にキャンセルしてください`
      case 'already_reverted':
        return `#${id} は全件戻し済み`
      case 'pending':
        return `#${id} の対象に反映待ちのトラックがあります。完了を待ってください`
      case 'no_changes':
        return `#${id} に戻す変更がありません`
    }
  }
  return e instanceof Error ? e.message : String(e)
}
