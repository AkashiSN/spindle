// YouTube 画面 ① の照合（D-87）: 入力が止まって 400ms で `POST /api/ytmusic/lookup`（DB だけ）を引き、
// 再生リストは URL ごとに 1 回だけ `POST /api/ytmusic/playlist`（yt-dlp で列挙。数秒かかる）を引く。
// 列挙の結果は URL をキーに持ち続ける（同じ再生リストを貼り直しても yt-dlp を呼び直さない）

import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError, apiPost } from '../api/client'
import type { LookupItem, PlaylistInfo, PlaylistProbe } from '../lib/youtube'

const DEBOUNCE_MS = 400
const EMPTY: ReadonlyMap<string, LookupItem> = new Map()

export type UrlLookup = {
  lookup: ReadonlyMap<string, LookupItem>
  probes: ReadonlyMap<string, PlaylistProbe>
  error: string | null
  /** 照合を引き直す（ダウンロードが済んで所在が変わったとき） */
  refresh: () => void
}

function message(e: unknown): string {
  if (e instanceof ApiError) return e.message
  return e instanceof Error ? e.message : String(e)
}

export function useUrlLookup(urls: readonly string[]): UrlLookup {
  const [lookup, setLookup] = useState<ReadonlyMap<string, LookupItem>>(new Map())
  const [probes, setProbes] = useState<ReadonlyMap<string, PlaylistProbe>>(new Map())
  const [error, setError] = useState<string | null>(null)
  const [tick, setTick] = useState(0)
  const started = useRef(new Set<string>())
  const seq = useRef(0)
  const key = urls.join('\n')

  const probe = useCallback((url: string) => {
    if (started.current.has(url)) return
    started.current.add(url)
    setProbes((m) => new Map(m).set(url, { state: 'loading' }))
    apiPost<PlaylistInfo>('/api/ytmusic/playlist', { url })
      .then((info) => setProbes((m) => new Map(m).set(url, { state: 'ok', info })))
      .catch((e: unknown) => {
        // 失敗は覚えない（次に貼り直したら引き直す）
        started.current.delete(url)
        setProbes((m) => new Map(m).set(url, { state: 'error', message: message(e) }))
      })
  }, [])

  useEffect(() => {
    if (key === '') return
    const list = key.split('\n')
    const my = ++seq.current
    const t = window.setTimeout(() => {
      apiPost<{ items: LookupItem[] }>('/api/ytmusic/lookup', { urls: list })
        .then((r) => {
          if (my !== seq.current) return
          setLookup(new Map(r.items.map((it, i) => [list[i], it])))
          setError(null)
          for (const it of r.items) if (it.kind === 'playlist') probe(it.url)
        })
        .catch((e: unknown) => {
          if (my === seq.current) setError(message(e))
        })
    }, DEBOUNCE_MS)
    return () => window.clearTimeout(t)
  }, [key, tick, probe])

  const refresh = useCallback(() => setTick((n) => n + 1), [])
  // 欄が空なら前の照合を見せない（effect で空にせず、描画で落とす）
  return { lookup: key === '' ? EMPTY : lookup, probes, error: key === '' ? null : error, refresh }
}
