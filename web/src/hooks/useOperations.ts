// 操作タブ（D-58）の状態: リネーム / 正規化の preview → apply、RG 解析 / 書き込み、FLAC 検査の投入。
//
// - preview は「種類・選択・ソート」の組に紐づき、どれかが変わると古くなる（[適用] は押せない）
// - 409 `pending` は一括編集と同じ「M 件を除外して適用 / 待つ」の 2 択。除外して適用は同じ操作を
//   `skip_pending` 付きでやり直す（リネーム / 正規化は同じ token、RG 書き込みは同じ selection）
// - 投入系（RG 解析 / FLAC 検査）は preview が無い。結果は件数を notice に出す
// - 埋め込み画像の差し替え（D-60）は upload → embed の 2 段。アップロード結果は選択に依らず残る
//   （同じ画像を別の集合へ繰り返し適用できる）
// - API はどれも既存（SPEC §9）。ここで新しい経路は作らない

import { useCallback, useMemo, useState } from 'react'
import { ApiError, parseErrorBody } from '../api/client'
import type { PendingConflict } from '../api/types'
import type { UploadedArtwork } from '../lib/artwork'
import {
  embedMessage,
  flaccheckStartedMessage,
  hirescheckStartedMessage,
  verifyStartedMessage,
  md5FillMessage,
  operationErrorMessage,
  rgStartedMessage,
  rgWrittenMessage,
  type EmbedResponse,
  type FlaccheckStartResponse,
  type HirescheckStartResponse,
  type VerifyStartResponse,
  type Md5FillResponse,
  type PathApplyResponse,
  type PathPreview,
  type RgStartResponse,
  type RgWriteResponse,
} from '../lib/operations'
import type { Selection } from '../lib/selection'
import { toSelectionBody } from '../lib/selection'

export type PathKind = 'rename' | 'normalize'

export type PathPreviewState = { kind: PathKind; key: string; preview: PathPreview }

export type OperationPending = {
  /** 除外して適用したときにやり直す操作 */
  action: 'paths' | 'rgwrite' | 'md5fill' | 'embed'
  count: number
  trackIds: number[]
  /** 確認を出したときの選択・ソート。変わっていれば出さない（確認した対象と適用対象がずれる） */
  key: string
}

export type Operations = {
  /** 実行中の操作（ボタンを無効にする）。無ければ null */
  busy: string | null
  notice: string | null
  error: string | null
  /** 現在の選択・ソートに対して有効な preview（古ければ null） */
  pathPreview: PathPreviewState | null
  /** preview したが選択・ソートが変わって古くなった種類（③ を「古い」にする。D-87）。無ければ null */
  pathStale: PathKind | null
  pendingPrompt: OperationPending | null
  previewPaths: (kind: PathKind) => Promise<void>
  /** 適用。投入できたら true */
  applyPaths: (description: string, skipPending?: boolean) => Promise<boolean>
  startRg: () => Promise<void>
  writeRg: (skipPending?: boolean) => Promise<void>
  startFlaccheck: () => Promise<void>
  /** 偽ハイレゾ検出（可逆かつ 48 kHz 超または 16 bit 超のトラックを解析。読むだけ） */
  startHirescheck: () => Promise<void>
  startVerify: () => Promise<void>
  /** MD5 の補填（md5_missing の FLAC に編集バッチ。409 pending は 2 択） */
  startMd5Fill: (skipPending?: boolean) => Promise<void>
  /** アップロード済みの画像（埋め込み差し替えの元）。無ければ null */
  uploaded: UploadedArtwork | null
  /** 画像をアップロードする（`POST /api/artwork/upload`）。成功すれば uploaded に入る */
  uploadArtwork: (file: File) => Promise<void>
  clearUploaded: () => void
  /** 選択トラックの埋め込み画像を uploaded に差し替える編集バッチ（409 pending は 2 択） */
  embedArtwork: (description: string, skipPending?: boolean) => Promise<boolean>
  dismissPending: () => void
  clearNotice: () => void
  /** 結果・失敗・反映待ちの確認をまとめて消す（操作タブで別の操作を選んだとき。前の操作の確認が
   *  別の操作の段に出て押されないように。D-87） */
  clearMessages: () => void
}

const PATH_URL: Record<PathKind, string> = { rename: '/api/rename', normalize: '/api/normalize' }

export function useOperations(selection: Selection, sortParam: string): Operations {
  const [busy, setBusy] = useState<string | null>(null)
  const [notice, setNotice] = useState<string | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [stored, setStored] = useState<PathPreviewState | null>(null)
  const [storedPending, setPendingPrompt] = useState<OperationPending | null>(null)
  const [uploaded, setUploaded] = useState<UploadedArtwork | null>(null)

  const sel = useMemo(() => toSelectionBody(selection), [selection])
  const selKey = useMemo(() => JSON.stringify([sel, sortParam]), [sel, sortParam])
  const pathPreview = stored != null && stored.key === `${stored.kind}|${selKey}` ? stored : null
  const pathStale = stored != null && pathPreview == null ? stored.kind : null
  // 409 pending の確認は出したときの選択に紐づける。選択が変わったら「除外して適用」が別の集合に
  // 効いてしまうので出さない
  const pendingPrompt = storedPending != null && storedPending.key === selKey ? storedPending : null

  const fail = useCallback((e: unknown) => {
    setError(e instanceof ApiError ? e.message : e instanceof Error ? e.message : String(e))
  }, [])

  const begin = useCallback((what: string) => {
    setBusy(what)
    setError(null)
    setNotice(null)
    setPendingPrompt(null)
  }, [])

  const previewPaths = useCallback(
    async (kind: PathKind) => {
      if (!sel) {
        setError('行を選択してください')
        return
      }
      begin(`preview:${kind}`)
      try {
        const r = await parseErrorBody<PathPreview>(`${PATH_URL[kind]}/preview`, {
          method: 'POST',
          body: JSON.stringify({ selection: sel, sort: sortParam }),
        })
        if (r.ok) setStored({ kind, key: `${kind}|${selKey}`, preview: r.body })
        else setError(operationErrorMessage(r.status, r.body))
      } catch (e) {
        fail(e)
      } finally {
        setBusy(null)
      }
    },
    [sel, sortParam, selKey, begin, fail],
  )

  const applyPaths = useCallback(
    async (description: string, skipPending = false): Promise<boolean> => {
      const pv = pathPreview
      if (!pv) {
        setError('先にプレビューしてください')
        return false
      }
      begin(`apply:${pv.kind}`)
      try {
        const r = await parseErrorBody<PathApplyResponse>(`${PATH_URL[pv.kind]}/apply`, {
          method: 'POST',
          body: JSON.stringify({
            selection_token: pv.preview.selection_token,
            description: description || undefined,
            skip_pending: skipPending,
          }),
        })
        if (r.ok) {
          setStored(null)
          const label = pv.kind === 'rename' ? 'リネーム' : '正規化'
          setNotice(
            `${label}を投入した: ${r.body.affected} 件（バッチ #${r.body.batch_id}）${r.body.conflict > 0 ? `、衝突で飛ばした ${r.body.conflict} 件` : ''}`,
          )
          return true
        }
        const body = r.body as { error?: string } | null
        if (r.status === 409 && body?.error === 'pending') {
          const p = r.body as PendingConflict
          setPendingPrompt({ action: 'paths', count: p.count, trackIds: p.track_ids, key: selKey })
          return false
        }
        if (r.status === 409 && body?.error === 'preview_stale') setStored(null)
        setError(operationErrorMessage(r.status, r.body))
      } catch (e) {
        fail(e)
      } finally {
        setBusy(null)
      }
      return false
    },
    [pathPreview, selKey, begin, fail],
  )

  const startRg = useCallback(async () => {
    if (!sel) {
      setError('行を選択してください')
      return
    }
    begin('rg')
    try {
      const r = await parseErrorBody<RgStartResponse>('/api/rg', {
        method: 'POST',
        body: JSON.stringify({ selection: sel }),
      })
      if (r.ok) setNotice(rgStartedMessage(r.body))
      else setError(operationErrorMessage(r.status, r.body))
    } catch (e) {
      fail(e)
    } finally {
      setBusy(null)
    }
  }, [sel, begin, fail])

  const writeRg = useCallback(
    async (skipPending = false) => {
      if (!sel) {
        setError('行を選択してください')
        return
      }
      begin('rgwrite')
      try {
        const r = await parseErrorBody<RgWriteResponse>('/api/rg/write', {
          method: 'POST',
          body: JSON.stringify({ selection: sel, skip_pending: skipPending }),
        })
        if (r.ok) {
          setNotice(rgWrittenMessage(r.body))
          return
        }
        const body = r.body as { error?: string } | null
        if (r.status === 409 && body?.error === 'pending') {
          const p = r.body as PendingConflict
          setPendingPrompt({ action: 'rgwrite', count: p.count, trackIds: p.track_ids, key: selKey })
          return
        }
        setError(operationErrorMessage(r.status, r.body))
      } catch (e) {
        fail(e)
      } finally {
        setBusy(null)
      }
    },
    [sel, selKey, begin, fail],
  )

  const startFlaccheck = useCallback(async () => {
    if (!sel) {
      setError('行を選択してください')
      return
    }
    begin('flaccheck')
    try {
      const r = await parseErrorBody<FlaccheckStartResponse>('/api/flaccheck', {
        method: 'POST',
        body: JSON.stringify({ selection: sel }),
      })
      if (r.ok) setNotice(flaccheckStartedMessage(r.body))
      else setError(operationErrorMessage(r.status, r.body))
    } catch (e) {
      fail(e)
    } finally {
      setBusy(null)
    }
  }, [sel, begin, fail])

  const startHirescheck = useCallback(async () => {
    if (!sel) {
      setError('行を選択してください')
      return
    }
    begin('hirescheck')
    try {
      const r = await parseErrorBody<HirescheckStartResponse>('/api/hirescheck', {
        method: 'POST',
        body: JSON.stringify({ selection: sel }),
      })
      if (r.ok) setNotice(hirescheckStartedMessage(r.body))
      else setError(operationErrorMessage(r.status, r.body))
    } catch (e) {
      fail(e)
    } finally {
      setBusy(null)
    }
  }, [sel, begin, fail])

  const startVerify = useCallback(async () => {
    if (!sel) {
      setError('行を選択してください')
      return
    }
    begin('verify')
    try {
      const r = await parseErrorBody<VerifyStartResponse>('/api/verify', {
        method: 'POST',
        body: JSON.stringify({ selection: sel }),
      })
      if (r.ok) setNotice(verifyStartedMessage(r.body))
      else setError(operationErrorMessage(r.status, r.body))
    } catch (e) {
      fail(e)
    } finally {
      setBusy(null)
    }
  }, [sel, begin, fail])

  const startMd5Fill = useCallback(
    async (skipPending = false) => {
      if (!sel) {
        setError('行を選択してください')
        return
      }
      begin('md5fill')
      try {
        const r = await parseErrorBody<Md5FillResponse>('/api/md5fill', {
          method: 'POST',
          body: JSON.stringify({ selection: sel, skip_pending: skipPending }),
        })
        if (r.ok) {
          setNotice(md5FillMessage(r.body))
          return
        }
        const body = r.body as { error?: string } | null
        if (r.status === 409 && body?.error === 'pending') {
          const p = r.body as PendingConflict
          setPendingPrompt({ action: 'md5fill', count: p.count, trackIds: p.track_ids, key: selKey })
          return
        }
        setError(operationErrorMessage(r.status, r.body))
      } catch (e) {
        fail(e)
      } finally {
        setBusy(null)
      }
    },
    [sel, selKey, begin, fail],
  )


  const uploadArtwork = useCallback(
    async (file: File) => {
      begin('upload')
      try {
        const r = await parseErrorBody<UploadedArtwork>('/api/artwork/upload', {
          method: 'POST',
          headers: { 'Content-Type': file.type || 'application/octet-stream' },
          body: file,
        })
        if (r.ok) setUploaded(r.body)
        else setError(operationErrorMessage(r.status, r.body))
      } catch (e) {
        fail(e)
      } finally {
        setBusy(null)
      }
    },
    [begin, fail],
  )

  const embedArtwork = useCallback(
    async (description: string, skipPending = false): Promise<boolean> => {
      if (!sel) {
        setError('行を選択してください')
        return false
      }
      if (!uploaded) {
        setError('先に画像をアップロードしてください')
        return false
      }
      begin('embed')
      try {
        const r = await parseErrorBody<EmbedResponse>('/api/artwork/embed', {
          method: 'POST',
          body: JSON.stringify({
            selection: sel,
            sha256: uploaded.sha256,
            description: description || undefined,
            skip_pending: skipPending,
          }),
        })
        if (r.ok) {
          setNotice(embedMessage(r.body))
          return true
        }
        const body = r.body as { error?: string } | null
        if (r.status === 409 && body?.error === 'pending') {
          const p = r.body as PendingConflict
          setPendingPrompt({ action: 'embed', count: p.count, trackIds: p.track_ids, key: selKey })
          return false
        }
        // 画像がキャッシュから消えていた（GC 等）。アップロードし直してもらう
        if (r.status === 404 && body?.error === 'artwork_not_found') setUploaded(null)
        setError(operationErrorMessage(r.status, r.body))
      } catch (e) {
        fail(e)
      } finally {
        setBusy(null)
      }
      return false
    },
    [sel, selKey, uploaded, begin, fail],
  )

  return {
    busy,
    notice,
    error,
    pathPreview,
    pathStale,
    pendingPrompt,
    previewPaths,
    applyPaths,
    startRg,
    writeRg,
    startFlaccheck,
    startHirescheck,
    startVerify,
    startMd5Fill,
    uploaded,
    uploadArtwork,
    clearUploaded: useCallback(() => setUploaded(null), []),
    embedArtwork,
    dismissPending: useCallback(() => setPendingPrompt(null), []),
    clearNotice: useCallback(() => setNotice(null), []),
    clearMessages: useCallback(() => {
      setNotice(null)
      setError(null)
      setPendingPrompt(null)
    }, []),
  }
}
