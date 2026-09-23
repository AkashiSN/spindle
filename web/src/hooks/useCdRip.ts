// CD の吸い出し（P2-5）: `POST /api/cd/rip` で rip ジョブを投入し、SSE の `job` イベント（App が
// `onJob` に回す）で進捗を追う。進捗の読み方と表示は lib/cdRip.ts。画面を開き直したときは
// `GET /api/cd/status` の `rip_job` から追い直す（`adopt`）。終わったらジョブの `note` / `last_error`
// を取って結果の一行にする

import { useCallback, useRef, useState } from 'react'
import { ApiError, apiFetch, apiPost } from '../api/client'
import type { JobEvent, JobList } from '../api/types'
import { finalizeDraft, type DiscDraft } from '../lib/cd'
import { ripErrorMessage, ripOutcome, ripProgressFrom, type RipProgress } from '../lib/cdRip'

export type CdRipState = {
  /** 追っているジョブ（無ければ null） */
  jobId: number | null
  running: boolean
  progress: RipProgress | null
  /** 終わったときの一行（「Inbox に置いた: CD/…」）。失敗なら null で error に入る */
  result: string | null
  error: string | null
  starting: boolean
  start: (toc: string, draft: DiscDraft) => Promise<void>
  onJob: (e: JobEvent) => void
  /**
   * `GET /api/cd/status` の `rip_job` を渡す。進行中のジョブがあれば追い直し、追っているのに status が
   * 「進行中なし」を返したら（終端の SSE を取りこぼした）ジョブの状態を取り直して終わらせる
   */
  adopt: (jobId: number | null | undefined) => void
  clear: () => void
}

export function useCdRip(): CdRipState {
  const [jobId, setJobId] = useState<number | null>(null)
  const [running, setRunning] = useState(false)
  const [progress, setProgress] = useState<RipProgress | null>(null)
  const [result, setResult] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [starting, setStarting] = useState(false)
  // SSE のコールバックは描画をまたいで呼ばれるので、追っているジョブは ref でも持つ
  const current = useRef<number | null>(null)
  const runningRef = useRef(false)

  const follow = useCallback((id: number) => {
    current.current = id
    setJobId(id)
    runningRef.current = true
    setRunning(true)
    setProgress(null)
    setResult(null)
    setError(null)
  }, [])

  const start = useCallback(
    async (toc: string, draft: DiscDraft) => {
      setStarting(true)
      setError(null)
      try {
        const r = await apiPost<{ job_id: number }>('/api/cd/rip', { toc, metadata: finalizeDraft(draft) })
        follow(r.job_id)
      } catch (e) {
        setError(e instanceof ApiError ? ripErrorMessage(e.code, e.message) : String(e))
      } finally {
        setStarting(false)
      }
    },
    [follow],
  )

  // ジョブの今の状態を取り、終わっていれば結果の一行にする。まだ走っていれば何もしない（SSE を待つ）。
  // 終端の SSE を受けたとき・追い直したとき・status が進行中なしを返したときに呼ぶ
  const reconcile = useCallback(async (id: number) => {
    try {
      const list = await apiFetch<JobList>('/api/jobs?type=rip')
      const job = list.items.find((j) => j.id === id)
      if (current.current !== id) return
      const out = ripOutcome(job)
      if (out.kind === 'running') return
      runningRef.current = false
      setRunning(false)
      if (out.kind === 'done') setResult(out.result)
      else setError(out.error)
    } catch (e) {
      if (current.current === id) setError(e instanceof Error ? e.message : String(e))
    }
  }, [])

  const onJob = useCallback(
    (e: JobEvent) => {
      if (current.current == null || e.id !== current.current) return
      const p = ripProgressFrom(e.detail)
      if (p != null) setProgress(p)
      if (e.state === 'done' || e.state === 'failed' || e.state === 'cancelled') {
        runningRef.current = false
        setRunning(false)
        void reconcile(e.id)
      }
    },
    [reconcile],
  )

  const adopt = useCallback(
    (id: number | null | undefined) => {
      if (id == null) {
        // 追っているのに進行中なしと言われた: 終端の SSE を取りこぼしている
        if (current.current != null && runningRef.current) void reconcile(current.current)
        return
      }
      if (current.current === id) return
      follow(id)
      // status を読んでから追い始めるまでに終わっていたら、SSE はもう来ない
      void reconcile(id)
    },
    [follow, reconcile],
  )

  const clear = useCallback(() => {
    current.current = null
    setJobId(null)
    runningRef.current = false
    setRunning(false)
    setProgress(null)
    setResult(null)
    setError(null)
  }, [])

  return { jobId, running, progress, result, error, starting, start, onJob, adopt, clear }
}
