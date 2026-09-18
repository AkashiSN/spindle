// 設定画面（SPEC §12.6）の状態: config.toml の原文、再スキャン / deep scan の投入、GC の preview → 実行、
// 退避台帳。画面を開いたときに取り、GC preview はボタンで取り直す

import { useCallback, useEffect, useState } from 'react'
import { ApiError, apiFetch, apiPost } from '../api/client'
import type { ArchivedEntry, GcPreview } from '../lib/settings'

export type SettingsState = {
  config: { path: string | null; text: string } | null
  archive: ArchivedEntry[] | null
  gcPreview: GcPreview | null
  busy: string | null
  notice: string | null
  error: string | null
  refresh: () => void
  startScan: (kind: 'incremental' | 'deep') => Promise<void>
  loadGcPreview: () => Promise<void>
  startGc: () => Promise<void>
  clearNotice: () => void
}

function describe(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.code === 'duplicate') return '同じジョブが既に待ち行列にある'
    if (e.code === 'gc_unavailable') return 'GC が使えない（root を開けていない）'
    return e.message
  }
  return e instanceof Error ? e.message : String(e)
}

export function useSettings(enabled: boolean): SettingsState {
  const [config, setConfig] = useState<{ path: string | null; text: string } | null>(null)
  const [archive, setArchive] = useState<ArchivedEntry[] | null>(null)
  const [gcPreview, setGcPreview] = useState<GcPreview | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)

  const refresh = useCallback(() => {
    apiFetch<{ path: string | null; text: string }>('/api/config')
      .then(setConfig)
      .catch((e: unknown) => setError(describe(e)))
    apiFetch<{ items: ArchivedEntry[] }>('/api/archive')
      .then((r) => setArchive(r.items))
      .catch((e: unknown) => setError(describe(e)))
  }, [])
  useEffect(() => {
    if (enabled) refresh()
  }, [enabled, refresh])

  const run = useCallback(async (what: string, f: () => Promise<string | null>) => {
    setBusy(what)
    setError(null)
    setNotice(null)
    try {
      const n = await f()
      if (n != null) setNotice(n)
    } catch (e) {
      setError(describe(e))
    } finally {
      setBusy(null)
    }
  }, [])

  const startScan = useCallback(
    (kind: 'incremental' | 'deep') =>
      run(`scan:${kind}`, async () => {
        const r = await apiPost<{ job_id: number }>('/api/scan', { kind })
        return `${kind === 'deep' ? 'deep scan' : '再スキャン'}を投入した（ジョブ #${r.job_id}）`
      }),
    [run],
  )
  const loadGcPreview = useCallback(
    () =>
      run('gc:preview', async () => {
        setGcPreview(await apiFetch<GcPreview>('/api/gc/preview'))
        return null
      }),
    [run],
  )
  const startGc = useCallback(
    () =>
      run('gc:start', async () => {
        const r = await apiPost<{ job_id: number }>('/api/gc', {})
        setGcPreview(null)
        return `GC を投入した（ジョブ #${r.job_id}）`
      }),
    [run],
  )

  return {
    config,
    archive,
    gcPreview,
    busy,
    notice,
    error,
    refresh,
    startScan,
    loadGcPreview,
    startGc,
    clearNotice: useCallback(() => setNotice(null), []),
  }
}
