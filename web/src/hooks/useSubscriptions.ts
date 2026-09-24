// 購読の節の状態（SPEC §12.6、D-78、P4-16）: 一覧（`GET /api/ytmusic/subscriptions`）と同期ジョブ
// （`GET /api/jobs?type=playlist_sync`）、登録 / 変更 / 削除 / 同期の投入

import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError, apiFetch, apiPatch, apiPost } from '../api/client'
import type { Job, JobList, Subscription, SubscriptionList } from '../api/types'

export type SubscriptionInput = {
  url: string
  albumartist: string
  album: string
  category: string | null
  align: boolean
  enabled: boolean
  max_enqueue: number
}

export type SubscriptionPatch = Partial<Omit<SubscriptionInput, 'url'>>

export interface SubscriptionsState {
  items: Subscription[] | null
  /** playlist_sync のジョブ（queued / running / 直近の終端） */
  jobs: Job[]
  busy: boolean
  error: string | null
  refresh: () => void
  /** 登録する。できたら登録した購読、できなければ null */
  create: (input: SubscriptionInput) => Promise<Subscription | null>
  update: (id: number, patch: SubscriptionPatch) => Promise<boolean>
  remove: (id: number) => Promise<boolean>
  sync: (id: number) => Promise<boolean>
}

function message(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.code === 'duplicate_list') return '同じ再生リストの購読があります'
    if (e.code === 'duplicate_target') return '同じ追記先（アルバムアーティスト + アルバム）の購読があります'
    if (e.code === 'duplicate') return 'この購読の同期は既に投入されています'
    if (e.code === 'sync_running') return '同期の実行中は変更できません（終わるか取り消してから）'
    if (e.code === 'not_found') return '購読が見つかりません（消えたか、YouTube 連携が無効）'
    if (e.message) return e.message
  }
  return e instanceof Error ? e.message : String(e)
}

/** `enabled` は YouTube 画面を表示中か（表示中だけ取る） */
export function useSubscriptions(enabled: boolean): SubscriptionsState {
  const [items, setItems] = useState<Subscription[] | null>(null)
  const [jobs, setJobs] = useState<Job[]>([])
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const timer = useRef<number | null>(null)
  const fetchNow = useCallback(() => {
    Promise.all([
      apiFetch<SubscriptionList>('/api/ytmusic/subscriptions'),
      apiFetch<JobList>('/api/jobs?type=playlist_sync'),
    ])
      .then(([l, j]) => {
        setItems(l.items)
        setJobs(j.items)
        setError(null)
      })
      .catch((e: unknown) => setError(message(e)))
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
  const run = useCallback(
    async (f: () => Promise<unknown>): Promise<boolean> => {
      setBusy(true)
      setError(null)
      try {
        await f()
        fetchNow()
        return true
      } catch (e) {
        setError(message(e))
        return false
      } finally {
        setBusy(false)
      }
    },
    [fetchNow],
  )
  const create = useCallback(
    async (input: SubscriptionInput) => {
      let sub: Subscription | null = null
      const ok = await run(async () => {
        sub = await apiPost<Subscription>('/api/ytmusic/subscriptions', input)
      })
      return ok ? sub : null
    },
    [run],
  )
  const update = useCallback(
    (id: number, patch: SubscriptionPatch) =>
      run(() => apiPatch<Subscription>(`/api/ytmusic/subscriptions/${id}`, patch)),
    [run],
  )
  const remove = useCallback(
    (id: number) => run(() => apiFetch<void>(`/api/ytmusic/subscriptions/${id}`, { method: 'DELETE' })),
    [run],
  )
  const sync = useCallback(
    (id: number) => run(() => apiFetch<void>(`/api/ytmusic/subscriptions/${id}/sync`, { method: 'POST' })),
    [run],
  )
  return { items, jobs, busy, error, refresh, create, update, remove, sync }
}
