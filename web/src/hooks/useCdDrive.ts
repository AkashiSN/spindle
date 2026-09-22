// CD ドライブの監視（P2-1）: CD 画面を開いている間 `GET /api/cd/status` を 2 秒間隔で取り、
// 新しいディスクの TOC が出たら `onNewDisc` に渡す（照会の自動起動は呼び側）。`POST /api/cd/eject` も束ねる。
// 「新しい」の判定は lib/cdDrive.ts の newDiscToc（同じディスクの間は 1 回だけ）

import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError, apiFetch, apiPost } from '../api/client'
import { newDiscToc, type DriveStatus } from '../lib/cdDrive'

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
    return e.message
  }
  return e instanceof Error ? e.message : String(e)
}

export function useCdDrive(active: boolean, onNewDisc: (toc: string) => void): CdDriveState {
  const [status, setStatus] = useState<DriveStatus | null>(null)
  const [unavailable, setUnavailable] = useState(false)
  const [ejecting, setEjecting] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const lastSeen = useRef<string | null>(null)
  const onNew = useRef(onNewDisc)
  useEffect(() => {
    onNew.current = onNewDisc
  }, [onNewDisc])

  const refresh = useCallback(async () => {
    try {
      const s = await apiFetch<DriveStatus>('/api/cd/status')
      setStatus(s)
      setUnavailable(false)
      const toc = newDiscToc(lastSeen.current, s)
      lastSeen.current = s.toc
      if (toc != null) onNew.current(toc)
    } catch (e) {
      if (e instanceof ApiError && e.code === 'cd_unavailable') {
        setUnavailable(true)
        return
      }
      setError(describe(e))
    }
  }, [])

  useEffect(() => {
    if (!active) return
    let stopped = false
    const tick = () => {
      if (!stopped) void refresh()
    }
    tick()
    const id = window.setInterval(tick, DRIVE_POLL_MS)
    return () => {
      stopped = true
      window.clearInterval(id)
    }
  }, [active, refresh])

  const eject = useCallback(async () => {
    setEjecting(true)
    setError(null)
    try {
      await apiPost<undefined>('/api/cd/eject', {})
      await refresh()
    } catch (e) {
      setError(describe(e))
    } finally {
      setEjecting(false)
    }
  }, [refresh])

  return { status, unavailable, ejecting, error, eject }
}
