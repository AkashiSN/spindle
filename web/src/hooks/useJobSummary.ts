import { useCallback, useEffect, useRef, useState } from 'react'
import { apiFetch } from '../api/client'
import type { JobList, JobSummary } from '../api/types'

/** 下部バーのジョブ要約。SSE job / batch のたびに取り直す（250ms で間引く） */
export function useJobSummary(enabled: boolean) {
  const [summary, setSummary] = useState<JobSummary | null>(null)
  const timer = useRef<number | null>(null)
  const fetchNow = useCallback(() => {
    apiFetch<JobList>('/api/jobs')
      .then((l) => setSummary(l.summary))
      .catch(() => {})
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
  return { summary, refresh }
}
