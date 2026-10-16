import { useEffect, useState } from 'react'

/** 現在時刻（ms）。`enabled` の間だけ `intervalMs` ごとに読み直す（止まっている間の値は古いまま） */
export function useNow(intervalMs: number, enabled: boolean): number {
  const [now, setNow] = useState(() => Date.now())
  useEffect(() => {
    if (!enabled) return
    const id = window.setInterval(() => setNow(Date.now()), intervalMs)
    return () => window.clearInterval(id)
  }, [intervalMs, enabled])
  return now
}
