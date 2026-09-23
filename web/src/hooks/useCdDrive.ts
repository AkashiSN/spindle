// CD ドライブの監視（P2-1）: CD 画面を開いている間 `GET /api/cd/status` を 2 秒間隔で取り、
// 新しいディスクの TOC が出たら `onNewDisc` に渡す（照会の自動起動は呼び側）。`POST /api/cd/eject` も束ねる。
// 「新しい」の判定は lib/cdDrive.ts の newDiscToc（同じディスクの間は 1 回だけ）。
// 応答は世代（lib/latest.ts）で最新だけ採用し、前回の応答待ちの間は次の周回を飛ばす。画面を閉じたら
// 進行中の応答は捨てる（閉じた後に onNewDisc が走らない）

import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError, apiFetch, apiPost } from '../api/client'
import { newDiscToc, type DriveStatus } from '../lib/cdDrive'
import { Latest } from '../lib/latest'

export const DRIVE_POLL_MS = 2000

export type CdDriveState = {
  /** まだ取れていなければ null（切断・未ログイン・ドライブ未配線） */
  status: DriveStatus | null
  /** ドライブが配線されていない（503 cd_unavailable）。表示を出さない */
  unavailable: boolean
  ejecting: boolean
  error: string | null
  eject: () => Promise<void>
}

function describe(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.code === 'cd_unavailable') return 'CD ドライブが使えない（配線されていない）'
    if (e.code === 'eject_failed') return `取り出せない: ${e.message}`
    if (e.code === 'ripping') return '吸い出し中は取り出せない'
    return e.message
  }
  return e instanceof Error ? e.message : String(e)
}

export function useCdDrive(active: boolean, onNewDisc: (toc: string, status: DriveStatus) => void): CdDriveState {
  const [status, setStatus] = useState<DriveStatus | null>(null)
  const [unavailable, setUnavailable] = useState(false)
  const [ejecting, setEjecting] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const lastSeen = useRef<string | null>(null)
  const onNew = useRef(onNewDisc)
  useEffect(() => {
    onNew.current = onNewDisc
  }, [onNewDisc])
  const gen = useRef(new Latest())
  const inflight = useRef(false)

  const refresh = useCallback(async () => {
    const id = gen.current.next()
    inflight.current = true
    try {
      const s = await apiFetch<DriveStatus>('/api/cd/status')
      if (!gen.current.isCurrent(id)) return
      setStatus(s)
      setUnavailable(false)
      const toc = newDiscToc(lastSeen.current, s)
      lastSeen.current = s.toc
      if (toc != null) onNew.current(toc, s)
    } catch (e) {
      if (!gen.current.isCurrent(id)) return
      if (e instanceof ApiError && e.code === 'cd_unavailable') {
        setUnavailable(true)
        return
      }
      setError(describe(e))
    } finally {
      if (gen.current.isCurrent(id)) inflight.current = false
    }
  }, [])

  useEffect(() => {
    if (!active) return
    const g = gen.current
    inflight.current = false
    const tick = () => {
      if (!inflight.current) void refresh()
    }
    tick()
    const id = window.setInterval(tick, DRIVE_POLL_MS)
    return () => {
      window.clearInterval(id)
      g.invalidate()
    }
  }, [active, refresh])

  const eject = useCallback(async () => {
    setEjecting(true)
    setError(null)
    try {
      await apiPost<undefined>('/api/cd/eject', {})
      // 取り出しの直後の状態を取り直す（進行中の周回があっても、こちらが最新になる）
      await refresh()
    } catch (e) {
      setError(describe(e))
    } finally {
      setEjecting(false)
    }
  }, [refresh])

  return { status, unavailable, ejecting, error, eject }
}
