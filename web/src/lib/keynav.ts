// 一覧のキーボード操作（SPEC §12.2「キーボード」、P4-17）。
//
// カーソル行の移動だけを純粋に扱う。選択の変化は lib/selection（clickRow / rangeSelect）が持ち、
// スクロールと描画は components/TrackTable が行う。
// 行は先頭から順に積まれる（hooks/useTracks）ので、カーソルは **読み込み済みの範囲**（0 .. loaded-1）
// の中でだけ動く。未読込の骨組み行には id が無く、選択に入れられないため

const NAV_KEYS = ['ArrowDown', 'ArrowUp', 'PageDown', 'PageUp', 'Home', 'End'] as const
export type NavKey = (typeof NAV_KEYS)[number]

/** KeyboardEvent.key が移動キーならその名前、そうでなければ null */
export function asNavKey(key: string): NavKey | null {
  return (NAV_KEYS as readonly string[]).includes(key) ? (key as NavKey) : null
}

/**
 * 移動後のカーソル index。読み込み済みの行が無ければ null。
 * `cursor` が null（まだ無い）なら下向きは先頭、上向きは読み込み済みの末尾から始める。
 * `pageRows` は 1 画面の行数（0 以下なら 1 行）
 */
export function moveCursor(key: NavKey, cursor: number | null, loaded: number, pageRows: number): number | null {
  if (loaded <= 0) return null
  const last = loaded - 1
  const page = Math.max(1, pageRows)
  const clamp = (i: number) => Math.min(last, Math.max(0, i))
  switch (key) {
    case 'Home':
      return 0
    case 'End':
      return last
    case 'ArrowDown':
      return cursor == null ? 0 : clamp(cursor + 1)
    case 'PageDown':
      return cursor == null ? 0 : clamp(cursor + page)
    case 'ArrowUp':
      return cursor == null ? last : clamp(cursor - 1)
    case 'PageUp':
      return cursor == null ? last : clamp(cursor - page)
  }
}

/**
 * 表示中の行に居るカーソルだけを有効にする。ソート・フィルタ・再取得で行が消えたら null
 * （消えた id を Space で選択に足したり、そこから移動を始めたりしない）
 */
export function resolveCursor(cursorId: number | null, order: readonly number[]): number | null {
  return cursorId != null && order.includes(cursorId) ? cursorId : null
}

/**
 * キーの発生元が入力部品（ボタン・チェックボックス・入力欄など）なら、一覧の操作にしない。
 * 表ルートの onKeyDown にはツールバーや行内のボタンからも bubbling で届くため
 */
export function isInteractiveTarget(t: { tagName: string; isContentEditable: boolean } | null): boolean {
  if (!t) return false
  return ['INPUT', 'BUTTON', 'SELECT', 'TEXTAREA'].includes(t.tagName) || t.isContentEditable
}
