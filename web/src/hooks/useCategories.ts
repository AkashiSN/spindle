// 統制語彙（GET/POST /api/categories）。CD 取り込みの確定フォームが配置先の category を選ぶ（D-67）

import { useCallback, useEffect, useState } from 'react'
import { ApiError, apiFetch, apiPost } from '../api/client'

export type Category = { id: number; name: string }

export function useCategories(enabled: boolean) {
  const [items, setItems] = useState<Category[]>([])
  const [error, setError] = useState<string | null>(null)
  const refresh = useCallback(() => {
    apiFetch<{ items: Category[] }>('/api/categories')
      .then((r) => {
        setItems(r.items)
        setError(null)
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
  }, [])
  useEffect(() => {
    if (enabled) refresh()
  }, [enabled, refresh])
  /** 追加して一覧へ足す。同じ語彙があればその旨のエラー */
  const create = useCallback(async (name: string): Promise<Category | null> => {
    try {
      const c = await apiPost<Category>('/api/categories', { name })
      setItems((prev) => [...prev, c].sort((a, b) => a.name.localeCompare(b.name)))
      setError(null)
      return c
    } catch (e) {
      if (e instanceof ApiError && e.code === 'duplicate') setError(`同じ語彙があります: ${name}`)
      else if (e instanceof ApiError && e.code === 'bad_request') setError('ディレクトリ名に使えない名前です')
      else setError(e instanceof Error ? e.message : String(e))
      return null
    }
  }, [])
  return { items, error, refresh, create }
}
