// CD 画面（SPEC §12.6、P2-3）の状態: TOC の入力 → MusicBrainz 照会 → 候補の選択。
// TOC の取得元はドライブ（P2-1 の GET /api/cd/status）に差し替える前提で、今は貼り付け

import { useCallback, useState } from 'react'
import { ApiError, apiPost } from '../api/client'
import {
  initialSelection,
  normalizeTocInput,
  outcomeAfterTocEdit,
  type LookupResponse,
  type ReleaseCandidate,
} from '../lib/cd'

export type CdLookupState = {
  toc: string
  setToc: (v: string) => void
  busy: boolean
  error: string | null
  result: LookupResponse | null
  /** 選んだ候補（result.candidates の添字） */
  selected: number | null
  select: (i: number | null) => void
  chosen: ReleaseCandidate | null
  lookup: () => Promise<void>
  reset: () => void
}

function describe(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.code === 'bad_request') return `TOC を読めない: ${e.message}`
    if (e.code === 'lookup_failed') return `MusicBrainz に届かない: ${e.message}`
    if (e.code === 'musicbrainz_unavailable')
      return 'MusicBrainz が一時的に使えない（負荷制限）。しばらく待って再試行。続くなら [musicbrainz] の設定を確認'
    return e.message
  }
  return e instanceof Error ? e.message : String(e)
}

export function useCdLookup(): CdLookupState {
  const [toc, setTocRaw] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [result, setResult] = useState<LookupResponse | null>(null)
  const [selected, setSelected] = useState<number | null>(null)

  const lookup = useCallback(async () => {
    const normalized = normalizeTocInput(toc)
    if (normalized === '') {
      setError('TOC を貼り付けてください')
      return
    }
    setBusy(true)
    setError(null)
    setResult(null)
    setSelected(null)
    try {
      const r = await apiPost<LookupResponse>('/api/cd/lookup', { toc: normalized })
      setResult(r)
      setSelected(initialSelection(r))
    } catch (e) {
      setError(describe(e))
    } finally {
      setBusy(false)
    }
  }, [toc])

  // TOC を編集したら古い候補を残さない（新しい入力の下に前の結果が見えるのを防ぐ）
  const setToc = useCallback(
    (v: string) => {
      const prev = { result, selected, error }
      const next = outcomeAfterTocEdit(toc, v, prev)
      setTocRaw(v)
      if (next !== prev) {
        setResult(next.result)
        setSelected(next.selected)
        setError(next.error)
      }
    },
    [toc, result, selected, error],
  )

  const reset = useCallback(() => {
    setResult(null)
    setSelected(null)
    setError(null)
  }, [])

  const chosen = result != null && selected != null ? (result.candidates[selected] ?? null) : null
  return { toc, setToc, busy, error, result, selected, select: setSelected, chosen, lookup, reset }
}
