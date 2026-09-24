// YouTube 画面の状態（SPEC §12.6、D-70 追記、P4-13）: URL 欄と投入。`/youtube?url=` で開かれたときは
// モジュール読み込み時に URL を控えておき（ログイン画面を挟んでも残る）、初期値にする

import { useCallback, useEffect, useRef, useState } from 'react'
import { apiFetch, parseErrorBody } from '../api/client'
import type { Job, JobList } from '../api/types'
import { operationErrorMessage, youtubeStartedMessage, type YoutubeStartResponse } from '../lib/operations'
import { urlFromLocation, ytdlJobs } from '../lib/youtube'

/** ブックマークレットの受け口。読み込み時に 1 度だけ見て、アドレス欄は `/` に戻す（再読込で二重に入れない） */
const initialUrl: string | null = (() => {
  if (typeof window === 'undefined') return null
  const url = urlFromLocation(window.location.pathname, window.location.search)
  if (url != null || window.location.pathname.replace(/\/+$/, '') === '/youtube') {
    window.history.replaceState(null, '', '/')
  }
  return url
})()

/** `/youtube?url=` で開かれたか（App が初期画面を YouTube にする） */
export const OPENED_WITH_URL = initialUrl != null

export interface YoutubeState {
  urls: string
  setUrls: (v: string) => void
  busy: boolean
  notice: string | null
  error: string | null
  /** ytdl ジョブの一覧（`GET /api/jobs?type=ytdl`。種別ごとの上限なので他種別に押し出されない）。
   *  まだ取れていなければ null */
  jobs: Job[] | null
  /** 一覧を取り直す（SSE job のたびに App が呼ぶ。250ms で間引く） */
  refresh: () => void
  /** 欄の URL を ytdl ジョブに投入する。投入できたら欄を空にして true */
  start: (urls: string[]) => Promise<boolean>
  /** 直近の投入で返ったジョブ id（③ ④ はこれと、その再生リストの展開で増えた子を追う。D-87） */
  sessionIds: number[]
  clearNotice: () => void
}

/** `enabled` は YouTube 画面を表示中か（表示中だけ取る） */
export function useYoutube(enabled: boolean): YoutubeState {
  const [urls, setUrls] = useState(initialUrl ?? '')
  const [busy, setBusy] = useState(false)
  const [notice, setNotice] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [jobs, setJobs] = useState<Job[] | null>(null)
  const [sessionIds, setSessionIds] = useState<number[]>([])
  const timer = useRef<number | null>(null)
  const fetchNow = useCallback(() => {
    apiFetch<JobList>('/api/jobs?type=ytdl')
      .then((l) => {
        setJobs(ytdlJobs(l.items))
        setError(null)
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
  }, [])
  const refresh = useCallback(() => {
    if (!enabled || timer.current != null) return
    timer.current = window.setTimeout(() => {
      timer.current = null
      fetchNow()
    }, 250)
  }, [enabled, fetchNow])
  useEffect(() => {
    if (enabled) fetchNow()
  }, [enabled, fetchNow])
  const start = useCallback(async (list: string[]) => {
    if (list.length === 0) {
      setError('URL を 1 行に 1 つ入れてください')
      return false
    }
    setBusy(true)
    setError(null)
    try {
      const r = await parseErrorBody<YoutubeStartResponse>('/api/ytmusic/download', {
        method: 'POST',
        body: JSON.stringify({ urls: list }),
      })
      if (r.ok) {
        setNotice(youtubeStartedMessage(r.body))
        setSessionIds([...new Set(r.body.job_ids)])
        setUrls('')
        fetchNow()
        return true
      }
      setError(operationErrorMessage(r.status, r.body))
      return false
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
      return false
    } finally {
      setBusy(false)
    }
  }, [fetchNow])
  const clearNotice = useCallback(() => setNotice(null), [])
  return { urls, setUrls, busy, notice, error, jobs, refresh, start, sessionIds, clearNotice }
}
