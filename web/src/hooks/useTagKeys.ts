// タグのキーの候補（GET /api/tags/keys。D-113）。入力欄にフォーカスしたときに取り、30 秒は使い回す。
// 取れなくても入力はできる（よく使うキーだけを候補に出す）

import { useCallback, useState } from 'react'
import { apiFetch } from '../api/client'
import { mergeTagKeys, type TagKeyUsage } from '../lib/tagKeys'
import { COMMON_KEYS } from '../lib/tagops'

const FRESH_MS = 30_000

// 画面の中の複数の入力欄で 1 回の取得を共有する
let cache: { at: number; items: TagKeyUsage[] } | null = null
let pending: Promise<TagKeyUsage[]> | null = null

function fetchKeys(): Promise<TagKeyUsage[]> {
  if (cache != null && Date.now() - cache.at < FRESH_MS) return Promise.resolve(cache.items)
  if (pending == null) {
    pending = apiFetch<{ items: TagKeyUsage[] }>('/api/tags/keys')
      .then((r) => {
        cache = { at: Date.now(), items: r.items }
        return r.items
      })
      .finally(() => {
        pending = null
      })
  }
  return pending
}

export function useTagKeys() {
  const [keys, setKeys] = useState<TagKeyUsage[]>(() => mergeTagKeys(cache?.items ?? [], COMMON_KEYS))
  const load = useCallback(() => {
    fetchKeys()
      .then((items) => setKeys(mergeTagKeys(items, COMMON_KEYS)))
      .catch(() => {
        // 候補が無くても入力はできる
      })
  }, [])
  return { keys, load }
}
