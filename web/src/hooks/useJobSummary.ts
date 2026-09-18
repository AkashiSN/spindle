import { useCallback, useEffect, useRef, useState } from 'react'
import { apiFetch, ApiError } from '../api/client'
import type { Job, JobList, JobSummary } from '../api/types'

export type JobsState = {
  /** ヘッダの要約。まだ取れていなければ null */
  summary: JobSummary | null
  /** 一覧（サーバの上限まで。SPEC §12.5 のジョブ画面用）。まだ取れていなければ null */
  items: Job[] | null
  /** 種別ごとの並列度 */
  concurrency: Record<string, number>
  error: string | null
  notice: string | null
  refresh: () => void
  cancel: (id: number) => Promise<void>
  retry: (id: number) => Promise<void>
}

/** ジョブの要約と一覧。SSE job / batch のたびに取り直す（250ms で間引く）。リロードしても DB の値で復元 */
export function useJobSummary(enabled: boolean): JobsState {
  const [summary, setSummary] = useState<JobSummary | null>(null)
  const [items, setItems] = useState<Job[] | null>(null)
  const [concurrency, setConcurrency] = useState<Record<string, number>>({})
  const [error, setError] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const timer = useRef<number | null>(null)
  const fetchNow = useCallback(() => {
    apiFetch<JobList>('/api/jobs')
      .then((l) => {
        setSummary(l.summary)
        setItems(l.items)
        setConcurrency(l.concurrency ?? {})
        setError(null)
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
  }, [])
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

  const act = useCallback(
    async (id: number, what: 'cancel' | 'retry') => {
      try {
        await apiFetch(`/api/jobs/${id}/${what}`, { method: 'POST' })
        setNotice(what === 'cancel' ? `#${id} の取り消しを要求した` : `#${id} を再試行に戻した`)
        fetchNow()
      } catch (e) {
        if (e instanceof ApiError) {
          const why =
            e.code === 'not_cancellable'
              ? '既に終わっている'
              : e.code === 'not_retryable'
                ? '失敗 / 取り消し以外は再試行できない'
                : e.code === 'duplicate'
                  ? '同じジョブが既に待ち行列にある'
                  : e.code === 'not_found'
                    ? '見つからない'
                    : e.message
          setNotice(`#${id}: ${why}`)
          fetchNow()
          return
        }
        setNotice(e instanceof Error ? e.message : String(e))
      }
    },
    [fetchNow],
  )

  return {
    summary,
    items,
    concurrency,
    error,
    notice,
    refresh,
    cancel: useCallback((id: number) => act(id, 'cancel'), [act]),
    retry: useCallback((id: number) => act(id, 'retry'), [act]),
  }
}
