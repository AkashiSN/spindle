// CD 画面の状態機械（SPEC §12.6、P2-3 / P2-4）。照会 → 候補の選択 → フォーム（候補を写したもの、または空）
// → 確定。純粋な reducer にして遷移を vitest で固定する（hook は非同期の照会と dispatch だけ）。
//
// 段: toc（入力）→ result（照会結果）→ selected / draft（選択とフォーム）→ confirmed（確定）。
// 上の段が変わると下の段は全部消える: TOC の編集と reset は結果ごと、照会のやり直しと候補の選び直しは
// 選択より下。貼り付け欄の本文はフォームの一部なので、TOC が変わる（別のディスク）と消える

import {
  applyTracklist,
  draftFromCandidate,
  emptyDraft,
  fillEmptyTitles,
  finalizeDraft,
  initialSelection,
  outcomeAfterTocEdit,
  validateDraft,
  type CopyScope,
  type DiscDraft,
  type DiscMetadata,
  type DiscTrackDraft,
  type LookupResponse,
} from './cd'
import { parseTracklist } from './tracklist'

export type CdState = {
  toc: string
  busy: boolean
  error: string | null
  result: LookupResponse | null
  /** 選んだ候補（result.candidates の添字）。手入力なら null */
  selected: number | null
  /** 候補から写す範囲（D-72、P4-2）。既定は識別用の最小限 */
  copyScope: CopyScope
  /** 編集中のフォーム（候補を選ぶか手入力を始めると現れる） */
  draft: DiscDraft | null
  paste: string
  pasteArtistFirst: boolean
  pasteWarnings: string[]
  /** 確定できない理由（confirm を押したとき） */
  draftErrors: string[]
  confirmed: DiscMetadata | null
}

export type CdAction =
  | { type: 'set_toc'; toc: string }
  | { type: 'lookup_start' }
  | { type: 'lookup_ok'; result: LookupResponse }
  | { type: 'lookup_error'; error: string }
  | { type: 'reset' }
  | { type: 'select'; index: number }
  | { type: 'set_copy_scope'; scope: CopyScope }
  | { type: 'start_manual' }
  | { type: 'update_draft'; patch: Partial<DiscDraft> }
  | { type: 'update_track'; index: number; patch: Partial<DiscTrackDraft> }
  | { type: 'fill_titles' }
  | { type: 'set_paste'; text: string }
  | { type: 'set_paste_artist_first'; value: boolean }
  | { type: 'apply_paste' }
  | { type: 'confirm' }
  | { type: 'unconfirm' }

export const initialCdState: CdState = {
  toc: '',
  busy: false,
  error: null,
  result: null,
  selected: null,
  copyScope: 'minimal',
  draft: null,
  paste: '',
  pasteArtistFirst: false,
  pasteWarnings: [],
  draftErrors: [],
  confirmed: null,
}

/** 選択より下（選択・フォーム・貼り付けの警告・確定）を消す。貼り付けの本文と向きは保つ */
function belowResult(s: CdState): CdState {
  return { ...s, selected: null, draft: null, pasteWarnings: [], draftErrors: [], confirmed: null }
}

/** 結果より下を全部消す（貼り付けの本文も。別のディスク） */
function belowToc(s: CdState): CdState {
  return { ...belowResult(s), result: null, error: null, paste: '' }
}

/** フォームを差し替える（貼り付けの警告・エラー・確定は消える） */
function withDraft(s: CdState, selected: number | null, draft: DiscDraft): CdState {
  return { ...s, selected, draft, pasteWarnings: [], draftErrors: [], confirmed: null }
}

export function cdReducer(s: CdState, a: CdAction): CdState {
  switch (a.type) {
    case 'set_toc': {
      // TOC を編集したら古い候補を残さない（新しい入力の下に前の結果が見えるのを防ぐ）
      const prev = { result: s.result, selected: s.selected, error: s.error }
      const next = outcomeAfterTocEdit(s.toc, a.toc, prev)
      if (next === prev) return { ...s, toc: a.toc }
      return { ...belowToc(s), toc: a.toc }
    }
    case 'lookup_start':
      return { ...belowResult(s), busy: true, error: null, result: null }
    case 'lookup_ok': {
      const r = a.result
      const sel = initialSelection(r)
      const base: CdState = { ...belowResult(s), busy: false, error: null, result: r }
      // exact が 1 件ならそれをフォームに、候補が無ければ空のフォームへ（照会ゼロ件でも完走できる。D-21）
      if (sel != null) return withDraft(base, sel, draftFromCandidate(r.candidates[sel]!, r.tracks, s.copyScope))
      if (r.candidates.length === 0) return withDraft(base, null, emptyDraft(r.tracks))
      return base
    }
    case 'lookup_error':
      return { ...belowResult(s), busy: false, error: a.error, result: null }
    case 'reset':
      return belowToc(s)
    case 'select': {
      // 候補を選ぶとフォームを写し直す（編集中の内容は捨てる。画面で断っている）。確定後は選び直せない
      if (s.result == null || s.confirmed != null) return s
      const c = s.result.candidates[a.index]
      if (c == null) return s
      return withDraft(s, a.index, draftFromCandidate(c, s.result.tracks, s.copyScope))
    }
    case 'set_copy_scope': {
      // 範囲を変えると、候補を選択中（未確定）ならその候補を写し直す（編集中の内容は捨てる。画面で断っている）。
      // 手入力中・確定後は範囲だけ変わる
      if (a.scope === s.copyScope) return s
      const next = { ...s, copyScope: a.scope }
      if (s.result == null || s.selected == null || s.confirmed != null) return next
      const c = s.result.candidates[s.selected]
      if (c == null) return next
      return withDraft(next, s.selected, draftFromCandidate(c, s.result.tracks, a.scope))
    }
    case 'start_manual':
      if (s.result == null || s.confirmed != null) return s
      return withDraft(s, null, emptyDraft(s.result.tracks))
    case 'update_draft':
      if (s.draft == null) return s
      return { ...s, draft: { ...s.draft, ...a.patch }, draftErrors: [] }
    case 'update_track': {
      if (s.draft == null) return s
      const tracks = s.draft.tracks.map((t, i) => (i === a.index ? { ...t, ...a.patch } : t))
      return { ...s, draft: { ...s.draft, tracks }, draftErrors: [] }
    }
    case 'fill_titles':
      if (s.draft == null) return s
      return { ...s, draft: fillEmptyTitles(s.draft), draftErrors: [] }
    case 'set_paste':
      return { ...s, paste: a.text }
    case 'set_paste_artist_first':
      return { ...s, pasteArtistFirst: a.value }
    case 'apply_paste': {
      if (s.draft == null) return s
      const parsed = parseTracklist(s.paste, { artistFirst: s.pasteArtistFirst })
      if (parsed.tracks.length === 0) return { ...s, pasteWarnings: ['貼り付けからトラックを読めない'] }
      const applied = applyTracklist(s.draft, parsed.tracks)
      return { ...s, draft: applied.draft, pasteWarnings: [...parsed.warnings, ...applied.warnings], draftErrors: [] }
    }
    case 'confirm': {
      if (s.draft == null) return s
      const errors = validateDraft(s.draft)
      return { ...s, draftErrors: errors, confirmed: errors.length === 0 ? finalizeDraft(s.draft) : null }
    }
    case 'unconfirm':
      return { ...s, confirmed: null }
  }
}
