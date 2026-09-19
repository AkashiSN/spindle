// Inbox タブ（SPEC §12.6、D-68）の状態。一覧は画面表示時と inbox ジョブの job イベントのたびに取り直す
// （250ms で間引く）。承認 / 却下 / 再開は成功したら一覧を取り直し、失敗は notice に出す

import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError, apiFetch, apiPost } from '../api/client'
import type { InboxDraft, InboxItem } from '../lib/inbox'

export type InboxState = {
  items: InboxItem[] | null
  /** 一覧取得のエラー。`unavailable` は [paths].inbox が無い（503） */
  error: string | null
  unavailable: boolean
  /** 直近の操作の結果 */
  notice: string | null
  busy: boolean
  refresh: () => void
  /** 走査を今すぐ投入する（走っていれば 409 → その旨を notice に） */
  scan: () => Promise<void>
  /** 承認して配置を投入する。失敗した理由（文字列）を返す。成功は null */
  approve: (id: number, draft: InboxDraft) => Promise<string | null>
  reject: (id: number) => Promise<void>
  reopen: (id: number) => Promise<void>
  setNotice: (s: string | null) => void
}

function describe(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.code === 'state') return '件の状態が変わっている（一覧を取り直した）'
    if (e.code === 'not_found') return '件が無くなっている（一覧を取り直した）'
    if (e.code === 'bad_request') return e.message
    if (e.code === 'duplicate') return '走査は既に投入されている'
    if (e.code === 'inbox_unavailable') return 'Inbox のディレクトリが設定されていない'
  }
  return e instanceof Error ? e.message : String(e)
}

export function useInbox(enabled: boolean): InboxState {
  const [items, setItems] = useState<InboxItem[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [unavailable, setUnavailable] = useState(false)
  const [notice, setNotice] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const timer = useRef<number | null>(null)

  const fetchNow = useCallback(() => {
    apiFetch<{ items: InboxItem[] }>('/api/inbox')
      .then((r) => {
        setItems(r.items)
        setError(null)
        setUnavailable(false)
      })
      .catch((e: unknown) => {
        setUnavailable(e instanceof ApiError && e.code === 'inbox_unavailable')
        setError(describe(e))
      })
  }, [])

  const refresh = useCallback(() => {
    if (timer.current != null) return
    timer.current = window.setTimeout(() => {
      timer.current = null
      fetchNow()
    }, 250)
  }, [fetchNow])

  useEffect(() => {
    if (enabled) fetchNow()
  }, [enabled, fetchNow])
  useEffect(
    () => () => {
      if (timer.current != null) window.clearTimeout(timer.current)
    },
    [],
  )

  /** 変更系の共通部分: 実行中フラグ、失敗の notice、終わったら一覧を取り直す */
  const run = useCallback(
    async (f: () => Promise<string | null>, onFail?: (msg: string) => void) => {
      setBusy(true)
      try {
        const msg = await f()
        if (msg != null) (onFail ?? setNotice)(msg)
        return msg
      } catch (e) {
        const msg = describe(e)
        ;(onFail ?? setNotice)(msg)
        return msg
      } finally {
        setBusy(false)
        fetchNow()
      }
    },
    [fetchNow],
  )

  const scan = useCallback(async () => {
    await run(async () => {
      await apiPost<{ job_id: number }>('/api/inbox/scan', {})
      setNotice('走査を投入した')
      return null
    })
  }, [run])

  const approve = useCallback(
    (id: number, draft: InboxDraft) =>
      run(
        async () => {
          await apiPost<{ job_id: number }>(`/api/inbox/${id}/approve`, draft)
          setNotice('承認した。配置はジョブで進む')
          return null
        },
        // 400 の理由はフォームの下に出す（呼び出し側が受け取る）
        () => {},
      ),
    [run],
  )

  const reject = useCallback(
    async (id: number) => {
      await run(async () => {
        await apiPost<undefined>(`/api/inbox/${id}/reject`, {})
        setNotice('却下した。ファイルは Inbox に残る')
        return null
      })
    },
    [run],
  )

  const reopen = useCallback(
    async (id: number) => {
      await run(async () => {
        await apiPost<undefined>(`/api/inbox/${id}/reopen`, {})
        setNotice('下書きに戻した')
        return null
      })
    },
    [run],
  )

  return { items, error, unavailable, notice, busy, refresh, scan, approve, reject, reopen, setNotice }
}
