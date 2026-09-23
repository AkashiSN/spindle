// CD 画面（SPEC §12.6、P2-3 / P2-4）の状態: 遷移は lib/cdState.ts の reducer（vitest で固定）。
// ここは非同期の照会（POST /api/cd/lookup）と dispatch の束ね。TOC はドライブ（P2-1 の
// GET /api/cd/status → useCdDrive → lookupToc）から来るのが本線で、貼り付け欄はドライブ無しの環境用。
// 取り込む内容は吸い出し（P2-5）に渡す。**この画面からは直せない**（P4-20 追記。補正は Inbox の
// 承認画面。D-67 追記）ので、ここが持つのは照会と候補の選択だけ

import { useCallback, useReducer, useRef } from 'react'
import { ApiError, apiPost } from '../api/client'
import {
  normalizeTocInput,
  type CopyScope,
  type LookupResponse,
  type ReleaseCandidate,
  type TocTrackInfo,
} from '../lib/cd'
import { cdReducer, initialCdState, type CdState } from '../lib/cdState'
import { Latest } from '../lib/latest'

/** TOC 以外の識別子（`GET /api/cd/status` の isrcs / mcn と、貼り付けたリリース URL / MBID） */
export type LookupExtra = {
  isrcs?: Array<string | null>
  mcn?: string | null
  release?: string | null
  /** サーバが覚えている結果を捨てて引き直す（画面のボタン。ディスク検出の自動照会は付けない） */
  refresh?: boolean
  /** 段を打ち切らずに全部引く（「さらに広げて探す」。D-64 追記 4） */
  widen?: boolean
}

export type CdLookupState = CdState & {
  setToc: (v: string) => void
  select: (i: number) => void
  setCopyScope: (scope: CopyScope) => void
  chosen: ReleaseCandidate | null
  lookup: () => Promise<void>
  /** TOC を欄に入れて照会する（ドライブの検出から。ISRC / MCN / 指定リリースも添えられる） */
  lookupToc: (toc: string, extra?: LookupExtra) => Promise<void>
  /** ドライブが読んだディスクを反映する（照会を待たずに表を出す） */
  setDisc: (toc: string, tracks: TocTrackInfo[]) => void
  /** 段を広げて引き直す（「さらに広げて探す」） */
  widen: () => Promise<void>
  reset: () => void
  startManual: () => void
}

/** 照会のエラーを画面の一行に（CD 画面と Inbox の引き直しで共有） */
export function describeLookupError(e: unknown): string {
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
  /** 直前の照会に添えた識別子（「さらに広げて探す」で同じものを使う） */
  const lastExtra = useRef<LookupExtra>({})

  const lookupToc = useCallback(async (toc: string, extra: LookupExtra = {}) => {
    const normalized = normalizeTocInput(toc)
    if (normalized === '') {
      gen.current.invalidate()
      dispatch({ type: 'lookup_error', error: 'TOC を貼り付けてください' })
      return
    }
    // 「さらに広げて探す」は直前と同じ識別子で引き直すので覚えておく
    lastExtra.current = extra
    const id = gen.current.next()
    dispatch({ type: 'set_toc', toc })
    dispatch({ type: 'lookup_start' })
    try {
      const r = await apiPost<LookupResponse>('/api/cd/lookup', {
        toc: normalized,
        isrcs: extra.isrcs ?? [],
        mcn: extra.mcn ?? null,
        release: extra.release ?? null,
        refresh: extra.refresh ?? false,
        widen: extra.widen ?? false,
      })
      if (gen.current.isCurrent(id)) dispatch({ type: 'lookup_ok', result: r })
    } catch (e) {
      if (gen.current.isCurrent(id)) dispatch({ type: 'lookup_error', error: describeLookupError(e) })
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
    setDisc: useCallback(
      (toc: string, tracks: TocTrackInfo[]) => dispatch({ type: 'set_disc', toc, tracks }),
      [],
    ),
    widen: useCallback(
      () => lookupToc(s.toc, { ...lastExtra.current, refresh: true, widen: true }),
      [lookupToc, s.toc],
    ),
  }
}
