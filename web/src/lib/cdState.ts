// CD 画面の状態機械（SPEC §12.6、P2-3 / P2-4 / P4-20）。純粋な reducer にして遷移を vitest で
// 固定する（hook は非同期の照会と dispatch だけ）。
//
// フォーム（draft）は **TOC が読めた時点で必ず存在する**。表は照会の前から出ていて、候補を選ぶと
// 名前が入る。だから draft が消えてよいのは「別のディスクに替わったとき」（set_disc / set_toc /
// reset）だけで、**照会の開始（lookup_start）と失敗（lookup_error）では消さない**。
// 消すと、照会が返るまで表が出ない／失敗で写した内容が飛ぶ。
//
// **CD 画面からは編集しない**（P4-20 追記）。draft を動かすのは「候補を選ぶ / 写す範囲を変える /
// どれも違う」だけで、値を直すのは Inbox の承認画面（D-67 追記）。だから update_draft のような
// 編集アクションも貼り付けも持たない。
//
// 照会をやり直すと消えるのは result と selected だけ（beforeLookup）。別のディスクに替わったときは
// busy も落とす（進行中の照会は hook 側が Latest で捨てるので、busy を残すと戻らない）

import {
  draftFromCandidate,
  emptyDraft,
  initialSelection,
  outcomeAfterTocEdit,
  type CopyScope,
  type DiscDraft,

  type LookupResponse,
  type TocTrackInfo,
} from './cd'

export type CdState = {
  toc: string
  busy: boolean
  error: string | null
  result: LookupResponse | null
  /** 選んだ候補（result.candidates の添字）。手入力なら null */
  selected: number | null
  /** 候補から写す範囲（D-72 追記 2、P4-2）。既定は「全部写す」 */
  copyScope: CopyScope
  /** 取り込む内容。TOC が読めた時点で必ずある（表は照会の前から出る）。画面からは直せない */
  draft: DiscDraft | null
}

export type CdAction =
  | { type: 'set_toc'; toc: string }
  /** ドライブが読んだディスク（TOC と音声トラック）。別のディスクならフォームごと作り直す */
  | { type: 'set_disc'; toc: string; tracks: TocTrackInfo[] }
  | { type: 'lookup_start' }
  | { type: 'lookup_ok'; result: LookupResponse }
  | { type: 'lookup_error'; error: string }
  | { type: 'reset' }
  | { type: 'select'; index: number }
  | { type: 'set_copy_scope'; scope: CopyScope }
  | { type: 'start_manual' }

export const initialCdState: CdState = {
  toc: '',
  busy: false,
  error: null,
  result: null,
  selected: null,
  // CD 画面の既定は「全部写す」（D-72 追記 2、P4-20）。表が主役なので、候補を選んだら名前が入る
  copyScope: 'full',
  draft: null,
}

/**
 * 照会をやり直すときに消すもの: 古い結果と選択だけ。**フォームは残す**
 * （表は照会の前から出ていて、失敗しても写した内容を飛ばさない）
 */
function beforeLookup(s: CdState): CdState {
  return { ...s, result: null, selected: null }
}

/**
 * 別のディスクに替わったときに消すもの（フォームも）。
 * busy も落とす: 進行中の照会は hook が Latest で捨てるので、残すと戻らなくなる
 */
function belowToc(s: CdState): CdState {
  return { ...beforeLookup(s), draft: null, error: null, busy: false }
}

/** フォームを差し替える */
function withDraft(s: CdState, selected: number | null, draft: DiscDraft): CdState {
  return { ...s, selected, draft }
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
    case 'set_disc': {
      // 同じディスクなら何もしない（2 秒ごとのポーリングで選んだ候補を消さない）
      if (s.toc === a.toc && s.draft != null) return s
      return { ...belowToc(s), toc: a.toc, draft: emptyDraft(a.tracks) }
    }
    case 'lookup_start':
      // draft は消さない（表は照会の前から出ている）
      return { ...beforeLookup(s), busy: true, error: null }
    case 'lookup_ok': {
      const r = a.result
      const sel = initialSelection(r)
      const base: CdState = { ...beforeLookup(s), busy: false, error: null, result: r }
      // exact が 1 件ならそれをフォームに写す。そうでなければ、TOC から作ってあるフォームのまま
      // （照会ゼロ件でも完走できる。D-21）
      if (sel != null) return withDraft(base, sel, draftFromCandidate(r.candidates[sel]!, r.tracks, s.copyScope))
      return withDraft(base, null, s.draft ?? emptyDraft(r.tracks))
    }
    case 'lookup_error':
      // 失敗しても表と写した内容は残す
      return { ...beforeLookup(s), busy: false, error: a.error }
    case 'reset':
      return belowToc(s)
    case 'select': {
      // 候補を選ぶとフォームを写し直す
      if (s.result == null) return s
      const c = s.result.candidates[a.index]
      if (c == null) return s
      return withDraft(s, a.index, draftFromCandidate(c, s.result.tracks, s.copyScope))
    }
    case 'set_copy_scope': {
      // 範囲を変えると、候補を選択中ならその候補を写し直す。「どれも違う」のときは範囲だけ変わる
      if (a.scope === s.copyScope) return s
      const next = { ...s, copyScope: a.scope }
      if (s.result == null || s.selected == null) return next
      const c = s.result.candidates[s.selected]
      if (c == null) return next
      return withDraft(next, s.selected, draftFromCandidate(c, s.result.tracks, a.scope))
    }
    case 'start_manual':
      if (s.result == null) return s
      return withDraft(s, null, emptyDraft(s.result.tracks))
  }
}
