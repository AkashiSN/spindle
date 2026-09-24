// Inbox の承認画面の配置先の見込み（④。`POST /api/inbox/:id/preview`、D-86）。下書きが変わるたびに
// 400ms 待ってから引く（打鍵ごとに引かない）。古い要求の応答は捨てる

import { useEffect, useState } from 'react'
import { apiPost } from '../api/client'
import { draftForSubmit, type InboxDraft } from '../lib/inbox'

export type InboxPreview = { rel_dir: string | null; paths: string[]; error: string | null }

export function useInboxPreview(itemId: number, draft: InboxDraft, enabled: boolean): InboxPreview | null {
  const body = JSON.stringify(draftForSubmit(draft))
  const key = `${itemId}\u0000${body}`
  const [result, setResult] = useState<{ key: string; value: InboxPreview } | null>(null)
  useEffect(() => {
    if (!enabled) return
    let live = true
    const timer = window.setTimeout(() => {
      apiPost<InboxPreview>(`/api/inbox/${itemId}/preview`, JSON.parse(body))
        .then((value) => {
          if (live) setResult({ key, value })
        })
        .catch((e: unknown) => {
          if (live) setResult({ key, value: { rel_dir: null, paths: [], error: e instanceof Error ? e.message : String(e) } })
        })
    }, 400)
    return () => {
      live = false
      window.clearTimeout(timer)
    }
  }, [itemId, body, key, enabled])
  return enabled && result?.key === key ? result.value : null
}
