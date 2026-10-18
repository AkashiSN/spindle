// Inbox の件の再判定（D-114）: 投入と進みの監視。ジョブは `inbox_rejudge`（payload.item_id が件）。
// `GET /api/jobs?type=inbox_rejudge` を 2 秒ごとに見て、走っていた件が終わったら `onDone(終わった件の id)` を
// 呼ぶ（一覧の取り直しと、移動先の件を開く処理は呼び出し側）。取得は順に番号を振り、投入より前に出した取得と
// 追い越された古い取得の結果は捨てる（投入した件がまだ載っていない一覧で「終わった」としない）

import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError, apiFetch, apiPost } from '../api/client'
import type { JobList } from '../api/types'

const POLL_MS = 2000

export type RejudgeProgress = { done: number; total: number }

export type InboxRejudge = {
  /** 件の再判定を投入する。失敗した理由（文字列）を返す。成功は null */
  start: (itemId: number) => Promise<string | null>
  /** 待ち / 実行中の件 id → 進み */
  running: Map<number, RejudgeProgress>
}

function describe(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.code === 'state') return '再判定できない状態になっている（一覧を取り直す）'
    if (e.code === 'not_found') return '取り込みが無くなっている（一覧を取り直す）'
    if (e.code === 'duplicate') return '再判定は既に投入されている'
  }
  return e instanceof Error ? e.message : String(e)
}

export function useInboxRejudge(onDone: (finished: number[]) => void): InboxRejudge {
  const [running, setRunning] = useState<Map<number, RejudgeProgress>>(new Map())
  const previous = useRef<Set<number>>(new Set())
  // 取得の番号: 最後に出した番号、最後に採った番号、採ってよい最小の番号（投入の後に出したもの）
  const issued = useRef(0)
  const applied = useRef(0)
  const minSeq = useRef(0)
  const onDoneRef = useRef(onDone)
  useEffect(() => {
    onDoneRef.current = onDone
  }, [onDone])

  const poll = useCallback(() => {
    const seq = ++issued.current
    apiFetch<JobList>('/api/jobs?type=inbox_rejudge')
      .then((l) => {
        if (seq < minSeq.current || seq <= applied.current) return
        applied.current = seq
        const now = new Map<number, RejudgeProgress>()
        for (const j of l.items) {
          if (j.type !== 'inbox_rejudge' || (j.state !== 'queued' && j.state !== 'running')) continue
          const id = j.payload?.item_id
          if (typeof id === 'number') now.set(id, { done: j.done ?? 0, total: j.total ?? 0 })
        }
        const finished = [...previous.current].filter((id) => !now.has(id))
        previous.current = new Set(now.keys())
        setRunning(now)
        if (finished.length > 0) onDoneRef.current(finished)
      })
      .catch(() => {
        // 一時的な取得失敗は次の周期で取り直す
      })
  }, [])

  useEffect(() => {
    poll()
    const t = window.setInterval(poll, POLL_MS)
    return () => window.clearInterval(t)
  }, [poll])

  const start = useCallback(
    async (itemId: number) => {
      try {
        await apiPost<{ job_id: number }>(`/api/inbox/${itemId}/rejudge`, {})
        // 次の取得までに終わっても「終わった」と気づけるよう、先に走っている扱いにする。投入の前に出した
        // 取得（この件がまだ載っていない）は捨てる
        previous.current.add(itemId)
        minSeq.current = issued.current + 1
        poll()
        return null
      } catch (e) {
        return describe(e)
      }
    },
    [poll],
  )

  return { start, running }
}
