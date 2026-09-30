// 端末タブとナビのバッジ（P5-2、D-95）。一覧は画面を開いていなくても 60 秒ごとに取る（バッジ）。
// 差分は端末タブで選んだ端末だけ取る。どちらもジョブの完了で取り直す（完了が続く間は待ち、静かになって
// から 3 秒後に 1 回。最長 10 秒で必ず取る。取るたびにサーバはハッシュの投入を確かめるので、完了のたびには取らない）

import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError, apiFetch, apiPatch, apiPost } from '../api/client'
import type {
  Device,
  DeviceDiff,
  DeviceList,
  DeviceSelection,
  DeviceVariant,
  SelectionEstimate,
  UnregisteredList,
} from '../api/types'
import { deviceMessage, diffFor, withDevice } from '../lib/devices'
import { mergePending, togglePlaylist } from '../lib/devicePicker'
import { withPlaylistIds } from '../lib/devicePicker'
import { Latest } from '../lib/latest'
import { TrailingDebounce } from '../lib/debounce'

const POLL_MS = 60_000
const JOB_REFRESH_WAIT_MS = 3000
const JOB_REFRESH_MAX_WAIT_MS = 10_000

export function useDevices(enabled: boolean, selectedId: number | null) {
  const [items, setItems] = useState<Device[] | null>(null)
  // 差分はどの端末のものかと組で持つ（端末を切り替えた直後に前の端末の差分を出さない）
  const [diffOf, setDiffOf] = useState<{ id: number; diff: DeviceDiff } | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const listGen = useRef(new Latest())
  const diffGen = useRef(new Latest())
  // 選んでいる端末。間引きの待ちや保存の途中で端末を切り替えても、取るのは常にいま選んでいる端末の差分
  // （作った時点の selectedId を閉じ込めた関数を後から呼ぶと、前の端末の要求が新しい端末の応答を捨てる）
  const selectedRef = useRef(selectedId)
  useEffect(() => {
    selectedRef.current = selectedId
  }, [selectedId])
  // 端末を切り替えたら前の端末のエラーは消す（描画中の調整。effect で setState しない）
  const [errorFor, setErrorFor] = useState(selectedId)
  if (errorFor !== selectedId) {
    setErrorFor(selectedId)
    setError(null)
  }

  // 端末の選曲（PUT は全置換）: 端末ごとに PUT を直列にし、本文は実行時点の「望む値」から作る。
  // 送信待ち・送信中の端末は、一覧の取り直しが古い値で楽観更新を潰さないよう望む値を重ねる
  const desired = useRef(new Map<number, number[]>())
  const pendingCount = useRef(new Map<number, number>())
  const chains = useRef(new Map<number, Promise<unknown>>())
  const itemsRef = useRef<Device[] | null>(null)
  useEffect(() => {
    itemsRef.current = items
  }, [items])

  const fetchList = useCallback(() => {
    const id = listGen.current.next()
    apiFetch<DeviceList>('/api/devices')
      .then((l) => {
        if (listGen.current.isCurrent(id)) setItems(mergePending(l.items, desired.current))
      })
      .catch(() => {
        if (listGen.current.isCurrent(id)) setItems(null)
      })
  }, [])
  const fetchDiff = useCallback(() => {
    const id = diffGen.current.next()
    const deviceId = selectedRef.current
    // 未選択なら取らない（返す diff は選んだ端末と組が合うときだけなので、前の値は出ない）
    if (deviceId == null) return
    apiFetch<DeviceDiff>(`/api/devices/${deviceId}/diff`)
      .then((d) => {
        if (diffGen.current.isCurrent(id)) setDiffOf({ id: deviceId, diff: d })
      })
      .catch((e: unknown) => {
        if (!diffGen.current.isCurrent(id)) return
        setDiffOf(null)
        // 消えた端末（別タブで削除など）は画面側が一覧から選び直すので、エラーには出さない
        if (!(e instanceof ApiError && e.code === 'not_found')) setError(deviceMessage(e))
      })
  }, [])
  // ジョブの完了による取り直しの間引き。fetchList / fetchDiff は固定なので一度だけ作る
  const jobRefresh = useRef<TrailingDebounce | null>(null)
  const jobDebounce = useCallback(() => {
    if (jobRefresh.current == null) {
      jobRefresh.current = new TrailingDebounce(
        () => {
          fetchList()
          fetchDiff()
        },
        JOB_REFRESH_WAIT_MS,
        JOB_REFRESH_MAX_WAIT_MS,
      )
    }
    return jobRefresh.current
  }, [fetchList, fetchDiff])
  /** 今すぐ取り直す（再接続など。間引きの予約は消す） */
  const refresh = useCallback(() => jobDebounce().flush(), [jobDebounce])
  /** ジョブの完了で取り直す（間引く） */
  const refreshAfterJob = useCallback(() => jobDebounce().trigger(), [jobDebounce])

  useEffect(() => {
    if (!enabled) return
    const g = listGen.current
    fetchList()
    const id = window.setInterval(fetchList, POLL_MS)
    return () => {
      window.clearInterval(id)
      // 間引きの待ちが残っていたら止める（アンマウント後に走らせない）
      jobRefresh.current?.cancel()
      g.invalidate()
    }
  }, [enabled, fetchList])
  useEffect(() => {
    if (!enabled) return
    const g = diffGen.current
    fetchDiff()
    return () => g.invalidate()
    // selectedId は fetchDiff が ref から読む。変わったら取り直すために依存に置く
  }, [enabled, selectedId, fetchDiff])

  // 選曲の見積もり（保存前の選び方で数える）。画面の effect の依存に入るので関数は固定する
  const estimate = useCallback(
    (id: number, selection: DeviceSelection, playlistIds: number[]) =>
      apiFetch<SelectionEstimate>(
        `/api/devices/${id}/estimate?selection=${selection}&playlist_ids=${playlistIds.join(',')}`,
      ),
    [],
  )

  const fetchUnregistered = useCallback(() => apiFetch<UnregisteredList>('/api/devices/adb/unregistered'), [])

  const run = useCallback(
    async (f: () => Promise<unknown>): Promise<boolean> => {
      setBusy(true)
      setError(null)
      try {
        await f()
        fetchList()
        fetchDiff()
        return true
      } catch (e) {
        setError(deviceMessage(e))
        // 差分が変わっていたら取り直す（画面の plan_token を古いまま持たない）
        if (e instanceof ApiError && (e.code === 'plan_changed' || e.code === 'open_plan_exists')) {
          fetchList()
          fetchDiff()
        }
        return false
      } finally {
        setBusy(false)
      }
    },
    [fetchList, fetchDiff],
  )

  const sendPlaylists = (id: number, playlistIds: number[]) =>
    apiFetch<Device>(`/api/devices/${id}/playlists`, {
      method: 'PUT',
      headers: { 'Content-Type': 'application/json' },
      body: JSON.stringify({ playlist_ids: playlistIds }),
    })
  const setPlaylists = (id: number, playlistIds: number[]): Promise<boolean> => {
    desired.current.set(id, playlistIds)
    setItems((cur) => withPlaylistIds(cur, id, playlistIds))
    pendingCount.current.set(id, (pendingCount.current.get(id) ?? 0) + 1)
    const task = (chains.current.get(id) ?? Promise.resolve()).then(async () => {
      const ids = desired.current.get(id)
      // 前の PUT が失敗して望む値が捨てられていたら、この分は送らない
      const ok = ids == null ? false : await run(() => sendPlaylists(id, ids))
      const left = (pendingCount.current.get(id) ?? 1) - 1
      if (!ok) desired.current.delete(id)
      if (left <= 0) {
        pendingCount.current.delete(id)
        desired.current.delete(id)
      } else pendingCount.current.set(id, left)
      if (!ok || left <= 0) fetchList()
      return ok
    })
    chains.current.set(id, task)
    return task
  }

  return {
    items,
    diff: diffFor(diffOf, selectedId),
    error,
    busy,
    refresh,
    refreshAfterJob,
    clearError: () => setError(null),
    /** iPhone（Mac の spindle-agent 経由）を登録する。成功すれば作った端末（選び直しに使う） */
    createIphone: async (name: string, variant: DeviceVariant, selection: DeviceSelection): Promise<Device | null> => {
      let created: Device | null = null
      const ok = await run(async () => {
        created = await apiPost<Device>('/api/devices', { name, transport: 'agent', variant, selection })
      })
      if (!ok || created == null) return null
      const d: Device = created
      // 一覧の取り直しを待たずに入れる（呼び出し側がすぐ選んでも、一覧に無い端末として選び直されない）
      setItems((cur) => withDevice(cur, d))
      return d
    },
    /** 未登録の Android（接続中）。登録のフォームを開いている間だけ取る */
    fetchUnregistered,
    /** Android を登録する。成功すれば作った端末 */
    registerAndroid: async (body: {
      name: string
      variant: DeviceVariant
      selection: DeviceSelection
      serial: string
      volume: string
    }): Promise<Device | null> => {
      let created: Device | null = null
      const ok = await run(async () => {
        created = await apiPost<Device>('/api/devices', { ...body, transport: 'adb' })
      })
      if (!ok || created == null) return null
      const d: Device = created
      setItems((cur) => withDevice(cur, d))
      return d
    },
    sync: (id: number, planToken: string) =>
      run(() => apiPost<{ job_id: number }>(`/api/devices/${id}/sync`, { plan_token: planToken })),
    resume: (id: number) => run(() => apiPost<{ job_id: number }>(`/api/devices/${id}/plans/open/resume`, {})),
    abandon: (id: number) => run(() => apiPost<Device>(`/api/devices/${id}/plans/open/abandon`, {})),
    verify: (id: number) => run(() => apiPost<{ job_id: number }>(`/api/devices/${id}/verify`, {})),
    update: (id: number, patch: { name?: string; selection?: DeviceSelection; variant?: DeviceVariant }) =>
      run(() => apiPatch<Device>(`/api/devices/${id}`, patch)),
    remove: (id: number) => run(() => apiFetch<void>(`/api/devices/${id}`, { method: 'DELETE' })),
    setPlaylists,
    /** 印を 1 つ付け外しする。続けて押しても、直前の望む値の上に積む */
    toggleDevicePlaylist: (id: number, playlistId: number): Promise<boolean> => {
      const d = itemsRef.current?.find((x) => x.id === id)
      const base = desired.current.get(id) ?? d?.playlist_ids
      if (!d || !base) return Promise.resolve(false)
      return setPlaylists(id, togglePlaylist({ ...d, playlist_ids: base }, playlistId))
    },
    estimate,
  }
}

export type Devices = ReturnType<typeof useDevices>
