// 上部バーの Inbox バッジ（P4-20）。承認待ちの件数だけを取る。
//
// 一覧（`GET /api/inbox`）は下書き・失敗理由まで読むので、画面を開いていなくても要るバッジの
// ために常時叩くには重い。`GET /api/inbox/summary` は固定 SQL の集計だけを返す。
// 更新は inbox ジョブ（走査・配置）の SSE で取り直し（250ms で間引く）、ほかに 60 秒の周期。

import { useCallback, useEffect, useRef, useState } from 'react'
import { apiFetch } from '../api/client'
import { Latest } from '../lib/latest'

export type InboxSummary = { pending: number; failed: number }

const POLL_MS = 60_000

export function useInboxSummary(enabled: boolean): {
  summary: InboxSummary | null
  refresh: () => void
} {
  const [summary, setSummary] = useState<InboxSummary | null>(null)
  const timer = useRef<number | null>(null)
  // 最新の要求の応答だけを採る（無効化・アンマウントの後の応答でバッジを書き換えない。
  // useCdLookup / useCdDrive と同じ流儀）
  const gen = useRef(new Latest())
  const fetchNow = useCallback(() => {
    const id = gen.current.next()
    apiFetch<InboxSummary>('/api/inbox/summary')
      .then((s) => {
        if (gen.current.isCurrent(id)) setSummary(s)
      })
      // [paths].inbox が無い（503）ならバッジを出さない
      .catch(() => {
        if (gen.current.isCurrent(id)) setSummary(null)
      })
  }, [])
  const refresh = useCallback(() => {
    if (timer.current != null) return
    timer.current = window.setTimeout(() => {
      timer.current = null
      fetchNow()
    }, 250)
  }, [fetchNow])
  useEffect(() => {
    if (!enabled) return
    const g = gen.current
    fetchNow()
    const id = window.setInterval(fetchNow, POLL_MS)
    return () => {
      window.clearInterval(id)
      // 間引きの待ちが残っていたら止める（アンマウント後に走らせない）
      if (timer.current != null) {
        window.clearTimeout(timer.current)
        timer.current = null
      }
      g.invalidate()
    }
  }, [enabled, fetchNow])
  return { summary, refresh }
}
