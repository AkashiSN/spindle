// CD 画面（SPEC §12.6、P2-3 / P2-4）の状態: 遷移は lib/cdState.ts の reducer（vitest で固定）。
// ここは非同期の照会（POST /api/cd/lookup）と dispatch の束ね。TOC はドライブ（P2-1 の
// GET /api/cd/status → useCdDrive → lookupToc）から来るのが本線で、貼り付け欄はドライブ無しの環境用。
// 確定したメタデータは吸い出し（P2-5）に渡す

import { useCallback, useReducer, useRef } from 'react'
import { ApiError, apiPost } from '../api/client'
import { normalizeTocInput, type CopyScope, type DiscDraft, type DiscTrackDraft, type LookupResponse, type ReleaseCandidate } from '../lib/cd'
import { cdReducer, initialCdState, type CdState } from '../lib/cdState'
import { Latest } from '../lib/latest'

export type CdLookupState = CdState & {
  setToc: (v: string) => void
  select: (i: number) => void
  setCopyScope: (scope: CopyScope) => void
  chosen: ReleaseCandidate | null
  lookup: () => Promise<void>
  /** TOC を欄に入れて照会する（ドライブの検出から） */
  lookupToc: (toc: string) => Promise<void>
  reset: () => void
  startManual: () => void
  updateDraft: (patch: Partial<DiscDraft>) => void
  updateTrack: (index: number, patch: Partial<DiscTrackDraft>) => void
  fillTitles: () => void
  setPaste: (v: string) => void
  setPasteArtistFirst: (v: boolean) => void
  applyPaste: () => void
  confirm: () => void
  /** 確定を取り消してフォームに戻る */
  unconfirm: () => void
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
  const [s, dispatch] = useReducer(cdReducer, initialCdState)
  // 照会の世代: 最新の要求の応答だけを reducer に入れる（ディスクを続けて替えたとき、古い TOC の候補が
  // 新しい TOC の下に居座らない）。TOC の編集と「結果を消す」も進行中の照会を無効にする
  const gen = useRef(new Latest())

  const lookupToc = useCallback(async (toc: string) => {
    const normalized = normalizeTocInput(toc)
    if (normalized === '') {
      gen.current.invalidate()
      dispatch({ type: 'lookup_error', error: 'TOC を貼り付けてください' })
      return
    }
    const id = gen.current.next()
    dispatch({ type: 'set_toc', toc })
    dispatch({ type: 'lookup_start' })
    try {
      const r = await apiPost<LookupResponse>('/api/cd/lookup', { toc: normalized })
      if (gen.current.isCurrent(id)) dispatch({ type: 'lookup_ok', result: r })
    } catch (e) {
      if (gen.current.isCurrent(id)) dispatch({ type: 'lookup_error', error: describe(e) })
    }
  }, [])
  const lookup = useCallback(() => lookupToc(s.toc), [lookupToc, s.toc])

  const chosen = s.result != null && s.selected != null ? (s.result.candidates[s.selected] ?? null) : null
  return {
    ...s,
    chosen,
    lookup,
    lookupToc,
    setToc: useCallback((toc: string) => {
      gen.current.invalidate()
      dispatch({ type: 'set_toc', toc })
    }, []),
    select: useCallback((index: number) => dispatch({ type: 'select', index }), []),
    setCopyScope: useCallback((scope: CopyScope) => dispatch({ type: 'set_copy_scope', scope }), []),
    reset: useCallback(() => {
      gen.current.invalidate()
      dispatch({ type: 'reset' })
    }, []),
    startManual: useCallback(() => dispatch({ type: 'start_manual' }), []),
    updateDraft: useCallback((patch: Partial<DiscDraft>) => dispatch({ type: 'update_draft', patch }), []),
    updateTrack: useCallback(
      (index: number, patch: Partial<DiscTrackDraft>) => dispatch({ type: 'update_track', index, patch }),
      [],
    ),
    fillTitles: useCallback(() => dispatch({ type: 'fill_titles' }), []),
    setPaste: useCallback((text: string) => dispatch({ type: 'set_paste', text }), []),
    setPasteArtistFirst: useCallback((value: boolean) => dispatch({ type: 'set_paste_artist_first', value }), []),
    applyPaste: useCallback(() => dispatch({ type: 'apply_paste' }), []),
    confirm: useCallback(() => dispatch({ type: 'confirm' }), []),
    unconfirm: useCallback(() => dispatch({ type: 'unconfirm' }), []),
  }
}
