// プロパティタブ用に `GET /api/tracks/:id` の detail を選択行の先頭 DETAIL_LIMIT 件だけ取る（D-58）。
// id ごとにキャッシュし、`version` が進んだら（バッチ適用・ライブラリ変更）全部捨てて取り直す

import { useEffect, useRef, useState } from 'react'
import { apiFetch } from '../api/client'
import type { TrackDetail, TrackWithDetail } from '../api/types'
import { DETAIL_LIMIT } from '../lib/properties'

export function useTrackDetails(
  ids: readonly number[],
  version: number,
  enabled: boolean,
): { details: ReadonlyMap<number, TrackDetail>; loading: boolean; error: string | null } {
  // キャッシュは ref に持ち、描画用の Map は取得が終わるたびに作り直す
  const cache = useRef<Map<number, TrackDetail>>(new Map())
  const cacheVersion = useRef(version)
  const [details, setDetails] = useState<ReadonlyMap<number, TrackDetail>>(() => new Map())
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  // 依存を id の並びの文字列にして、同じ選択で配列だけ作り直されたときに取り直さない
  const wantedKey = ids.slice(0, DETAIL_LIMIT).join(',')

  useEffect(() => {
    if (!enabled) return
    if (cacheVersion.current !== version) {
      cacheVersion.current = version
      cache.current = new Map()
      setDetails(new Map())
    }
    const wanted = wantedKey === '' ? [] : wantedKey.split(',').map(Number)
    const missing = wanted.filter((id) => !cache.current.has(id))
    if (missing.length === 0) return
    let cancelled = false
    setLoading(true)
    Promise.all(missing.map((id) => apiFetch<TrackWithDetail>(`/api/tracks/${id}`).catch(() => null)))
      .then((got) => {
        if (cancelled) return
        for (const t of got) if (t) cache.current.set(t.id, t.detail)
        setDetails(new Map(cache.current))
        setError(got.some((t) => t == null) ? '一部の詳細を取得できなかった' : null)
      })
      .finally(() => {
        if (!cancelled) setLoading(false)
      })
    return () => {
      cancelled = true
    }
  }, [wantedKey, version, enabled])

  return { details, loading, error }
}
