// 端末タブとナビのバッジ（P5-2、D-95）。一覧は画面を開いていなくても 60 秒ごとに取る（バッジ）。
// 差分は端末タブで選んだ端末だけ取る。どちらもジョブの完了で取り直す（250ms で間引く）

import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError, apiFetch, apiPatch, apiPost } from '../api/client'
import type { Device, DeviceDiff, DeviceList, DeviceSelection, DeviceVariant, SelectionEstimate } from '../api/types'
import { deviceMessage, diffFor, withDevice } from '../lib/devices'
import { withPlaylistIds } from '../lib/devicePicker'
import { Latest } from '../lib/latest'

const POLL_MS = 60_000

export function useDevices(enabled: boolean, selectedId: number | null) {
  const [items, setItems] = useState<Device[] | null>(null)
  // 差分はどの端末のものかと組で持つ（端末を切り替えた直後に前の端末の差分を出さない）
  const [diffOf, setDiffOf] = useState<{ id: number; diff: DeviceDiff } | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const listGen = useRef(new Latest())
  const diffGen = useRef(new Latest())
  const timer = useRef<number | null>(null)
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

  const fetchList = useCallback(() => {
    const id = listGen.current.next()
    apiFetch<DeviceList>('/api/devices')
      .then((l) => {
        if (listGen.current.isCurrent(id)) setItems(l.items)
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
  const refresh = useCallback(() => {
    if (timer.current != null) return
    timer.current = window.setTimeout(() => {
      timer.current = null
      fetchList()
      fetchDiff()
    }, 250)
  }, [fetchList, fetchDiff])

  useEffect(() => {
    if (!enabled) return
    const g = listGen.current
    fetchList()
    const id = window.setInterval(fetchList, POLL_MS)
    return () => {
      window.clearInterval(id)
      // 間引きの待ちが残っていたら止める（アンマウント後に走らせない）
      if (timer.current != null) {
        window.clearTimeout(timer.current)
        timer.current = null
      }
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
        return false
      } finally {
        setBusy(false)
      }
    },
    [fetchList, fetchDiff],
  )

  return {
    items,
    diff: diffFor(diffOf, selectedId),
    error,
    busy,
    refresh,
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
    update: (id: number, patch: { name?: string; selection?: DeviceSelection; variant?: DeviceVariant }) =>
      run(() => apiPatch<Device>(`/api/devices/${id}`, patch)),
    remove: (id: number) => run(() => apiFetch<void>(`/api/devices/${id}`, { method: 'DELETE' })),
    setPlaylists: (id: number, playlistIds: number[]) => {
      // PUT は全置換なので、完了前に続けて切り替えても前の印を落とさないよう、先に一覧へ反映する。失敗したら取り直して戻す
      setItems((cur) => withPlaylistIds(cur, id, playlistIds))
      return run(() =>
        apiFetch<Device>(`/api/devices/${id}/playlists`, {
          method: 'PUT',
          headers: { 'Content-Type': 'application/json' },
          body: JSON.stringify({ playlist_ids: playlistIds }),
        }),
      ).then((ok) => {
        if (!ok) fetchList()
        return ok
      })
    },
    estimate,
  }
}

export type Devices = ReturnType<typeof useDevices>
