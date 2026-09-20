// 一括編集の状態（SPEC §12.3、D-42）: 操作リスト、プレビュー結果、適用。
//
// - 操作リストは選択を変えても残る。プレビュー結果は「選択・操作・ソート」の組に紐づき、
//   どれかが変わると古くなる（表の差分表示は消え、[適用] はプレビューし直すまで押せない）
// - 適用は token の集合だけに効く。409 `pending` は「M 件を除外して適用 / 待つ」の 2 択にする
// - インライン編集は 1 件のバッチとして同じ経路（preview → apply をここで続けて呼ぶ）

import { useCallback, useMemo, useState } from 'react'
import { ApiError, apiPost, parseErrorBody } from '../api/client'
import type { ApplyResponse, PendingConflict, PreviewResponse } from '../api/types'
import {
  indexPreview,
  inlineEditOps,
  previewKey,
  visiblePendingPrompt,
  type PendingPrompt,
  type PreviewState,
} from '../lib/preview'
import type { Selection } from '../lib/selection'
import { toSelectionBody } from '../lib/selection'
import { newOp, opsToRequest, validateOps, type OpRequest, type TagOp } from '../lib/tagops'

export type BatchStatus = 'idle' | 'previewing' | 'applying'

export type { PendingPrompt }

export type BatchEdit = {
  ops: readonly TagOp[]
  setOps: (ops: readonly TagOp[]) => void
  /** 現在の選択・操作・ソートに対して有効なプレビュー（古ければ null） */
  preview: PreviewState | null
  /** 直近のプレビュー（古くても持つ。表の差分は `preview` が有効なときだけ出す） */
  status: BatchStatus
  error: string | null
  pendingPrompt: PendingPrompt | null
  runPreview: () => Promise<void>
  /** 適用。`skipPending` は 409 `pending` の後に「除外して適用」を選んだとき */
  apply: (description: string, skipPending?: boolean) => Promise<ApplyResponse | null>
  dismissPending: () => void
  /** インライン編集: 1 件を preview → apply する。失敗の理由を返す（成功なら null） */
  applyInline: (trackId: number, columnId: string, value: string) => Promise<string | null>
  /**
   * プロパティタブの編集: 現在の選択全体に `set key = values` の 1 op を preview → apply する
   * （D-58）。空の values は削除。失敗の理由を返す（成功なら null）
   */
  applyToSelection: (key: string, values: string[]) => Promise<string | null>
  /** プロパティタブの「フィールドを削除」（選択全体への 1 op の delete。P4-3）。失敗の理由を返す */
  deleteFromSelection: (key: string) => Promise<string | null>
  lastBatchId: number | null
}

export function useBatchEdit(selection: Selection, sortParam: string): BatchEdit {
  const [ops, setOps] = useState<readonly TagOp[]>(() => [newOp('set')])
  const [result, setResult] = useState<PreviewState | null>(null)
  const [status, setStatus] = useState<BatchStatus>('idle')
  const [error, setError] = useState<string | null>(null)
  // 409 pending の確認はプレビューの key に紐づけ、プレビューが古くなったら出さない
  const [storedPrompt, setPendingPrompt] = useState<PendingPrompt | null>(null)
  const [lastBatchId, setLastBatchId] = useState<number | null>(null)

  const request: OpRequest[] = useMemo(() => opsToRequest(ops), [ops])
  const key = useMemo(() => previewKey(selection, request, sortParam), [selection, request, sortParam])
  const preview = result != null && result.key === key ? result : null
  const pendingPrompt = visiblePendingPrompt(storedPrompt, key)

  const runPreview = useCallback(async () => {
    setError(null)
    setPendingPrompt(null)
    const problem = validateOps(ops)
    if (problem) {
      setError(problem)
      return
    }
    const sel = toSelectionBody(selection)
    if (!sel) {
      setError('行を選択してください')
      return
    }
    setStatus('previewing')
    try {
      const res = await apiPost<PreviewResponse>('/api/tracks/batch/preview', {
        selection: sel,
        ops: request,
        sort: sortParam,
      })
      setResult(indexPreview(res, key))
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e))
    } finally {
      setStatus('idle')
    }
  }, [ops, request, selection, sortParam, key])

  const apply = useCallback(
    async (description: string, skipPending = false): Promise<ApplyResponse | null> => {
      setPendingPrompt(null)
      if (!preview) {
        setError('先にプレビューしてください')
        return null
      }
      setError(null)
      setStatus('applying')
      try {
        const r = await parseErrorBody<ApplyResponse>('/api/tracks/batch', {
          method: 'PATCH',
          body: JSON.stringify({
            selection_token: preview.token,
            ops: request,
            description: description || undefined,
            skip_pending: skipPending,
          }),
        })
        if (r.ok) {
          setResult(null)
          setLastBatchId(r.body.batch_id)
          return r.body
        }
        const body = r.body as { error?: string; message?: string } | null
        if (r.status === 409 && body?.error === 'pending') {
          const p = body as PendingConflict
          setPendingPrompt({ key: preview.key, count: p.count, trackIds: p.track_ids })
          return null
        }
        if (r.status === 409 && body?.error === 'preview_stale') {
          setResult(null)
          setError('プレビューが古くなりました。もう一度プレビューしてください')
          return null
        }
        if (r.status === 409 && body?.error === 'no_changes') {
          setError('変更がありません')
          return null
        }
        setError(body?.message ?? `${body?.error ?? 'http_error'} (HTTP ${r.status})`)
        return null
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e))
        return null
      } finally {
        setStatus('idle')
      }
    },
    [preview, request],
  )

  const dismissPending = useCallback(() => setPendingPrompt(null), [])

  /** preview → apply を続けて呼ぶ（インライン編集・プロパティ編集）。反映待ちを含む集合は断る */
  const quickApply = useCallback(
    async (
      sel: { ids: number[] } | { filter: string; exclude_ids: number[] },
      ops: OpRequest[],
    ): Promise<string | null> => {
      try {
        const pv = await apiPost<PreviewResponse>('/api/tracks/batch/preview', { selection: sel, ops })
        if (pv.pending_excluded > 0) {
          return `反映待ちの行（${pv.pending_excluded} 件）を含むので編集できません。一括編集タブから除外して適用できます`
        }
        if (pv.changed === 0) return null
        const r = await parseErrorBody<ApplyResponse>('/api/tracks/batch', {
          method: 'PATCH',
          body: JSON.stringify({ selection_token: pv.selection_token, ops }),
        })
        if (r.ok) {
          setLastBatchId(r.body.batch_id)
          return null
        }
        const body = r.body as { error?: string; message?: string } | null
        if (body?.error === 'pending') return '反映待ちの行は編集できません'
        return body?.message ?? `${body?.error ?? 'http_error'} (HTTP ${r.status})`
      } catch (e) {
        if (e instanceof ApiError) return e.message
        return e instanceof Error ? e.message : String(e)
      }
    },
    [],
  )

  const applyInline = useCallback(
    async (trackId: number, columnId: string, value: string): Promise<string | null> => {
      const inline = inlineEditOps(columnId, value)
      if (!inline) return 'この列は編集できません'
      return quickApply({ ids: [trackId] }, inline)
    },
    [quickApply],
  )

  const applyToSelection = useCallback(
    async (key: string, values: string[]): Promise<string | null> => {
      const sel = toSelectionBody(selection)
      if (!sel) return '行を選択してください'
      const op: TagOp = { id: 'props', op: 'set', key, value: values.join('; ') }
      const problem = validateOps([op])
      if (problem) return problem.replace(/^1: /, '')
      const req = opsToRequest([op])[0]
      if (!req || req.op !== 'set') return '操作を組み立てられません'
      // 多値は配列で送る
      return quickApply(sel, [{ op: 'set', key: req.key, value: values }])
    },
    [selection, quickApply],
  )

  const deleteFromSelection = useCallback(
    async (key: string): Promise<string | null> => {
      const sel = toSelectionBody(selection)
      if (!sel) return '行を選択してください'
      const op: TagOp = { id: 'props', op: 'delete', key }
      const problem = validateOps([op])
      if (problem) return problem.replace(/^1: /, '')
      const req = opsToRequest([op])[0]
      if (!req || req.op !== 'delete') return '操作を組み立てられません'
      return quickApply(sel, [req])
    },
    [selection, quickApply],
  )

  return {
    ops,
    setOps,
    preview,
    status,
    error,
    pendingPrompt,
    runPreview,
    apply,
    dismissPending,
    applyInline,
    applyToSelection,
    deleteFromSelection,
    lastBatchId,
  }
}
