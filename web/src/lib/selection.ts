// 表の選択（SPEC §12.2、D-33、D-40）。
//
// 選択は **immutable な集合の記述** で、表示中の行とは独立に持つ:
//   - ids 形:    クリック / Shift / Ctrl で作った id の集合
//   - filter 形: Ctrl+A。「選択した時点のフィルタ式」+ 除外 id。6 万件の id を持ち歩かない
// 表示フィルタやソートを後から変えても、この値は変わらない（SSE でも変えない）

export type Selection =
  | { kind: 'none' }
  | { kind: 'ids'; ids: ReadonlySet<number>; anchor: number | null }
  | {
      kind: 'filter'
      /** filterToParam() の文字列。サーバの selection.filter にそのまま渡す */
      filter: string
      excludeIds: ReadonlySet<number>
      anchor: number | null
    }

export const NO_SELECTION: Selection = { kind: 'none' }

export type ClickModifiers = { shift: boolean; ctrl: boolean }

/** 表示中の行の id 列（読み込み済みの範囲）。Shift 範囲の解決に使う */
export type VisibleOrder = readonly number[]

function rangeIds(order: VisibleOrder, a: number, b: number): number[] | null {
  const ia = order.indexOf(a)
  const ib = order.indexOf(b)
  if (ia < 0 || ib < 0) return null
  const [lo, hi] = ia <= ib ? [ia, ib] : [ib, ia]
  return order.slice(lo, hi + 1)
}

/**
 * 行クリック。Shift は anchor からの範囲、Ctrl はトグル、素のクリックは置き換え。
 *
 * `displayFilter` は表示中のフィルタ文字列。filter 形の選択を持ったまま表示フィルタを変えた
 * あとは、表示中の行が選択集合に入っているかを client で判定できないので、Ctrl / Shift による
 * 除外・解除は **無視**する（除外 id を増やすと件数の引き算が合わなくなる。D-40）。
 * 素のクリックは ids 形への置き換えなので常に効く
 */
export function clickRow(
  sel: Selection,
  id: number,
  mods: ClickModifiers,
  order: VisibleOrder,
  displayFilter?: string,
): Selection {
  if (
    sel.kind === 'filter' &&
    (mods.shift || mods.ctrl) &&
    displayFilter !== undefined &&
    displayFilter !== sel.filter
  ) {
    return sel
  }
  const anchor = sel.kind === 'none' ? null : sel.anchor
  if (mods.shift && anchor != null) {
    const range = rangeIds(order, anchor, id)
    if (range) {
      if (sel.kind === 'filter') {
        const excludeIds = new Set(sel.excludeIds)
        for (const r of range) excludeIds.delete(r)
        return { ...sel, excludeIds, anchor }
      }
      const ids = new Set(sel.kind === 'ids' ? sel.ids : [])
      for (const r of range) ids.add(r)
      return { kind: 'ids', ids, anchor }
    }
    // anchor が表示中に無い（ソート・フィルタで消えた）ときは素のクリック扱い
  }
  if (mods.ctrl) {
    if (sel.kind === 'filter') {
      const excludeIds = new Set(sel.excludeIds)
      if (excludeIds.has(id)) excludeIds.delete(id)
      else excludeIds.add(id)
      return { ...sel, excludeIds, anchor: id }
    }
    const ids = new Set(sel.kind === 'ids' ? sel.ids : [])
    if (ids.has(id)) ids.delete(id)
    else ids.add(id)
    return ids.size === 0 ? NO_SELECTION : { kind: 'ids', ids, anchor: id }
  }
  return { kind: 'ids', ids: new Set([id]), anchor: id }
}

/** Ctrl+A。表示中のフィルタ式を **その時点の値で** 固定する */
export function selectAll(filterParam: string): Selection {
  return { kind: 'filter', filter: filterParam, excludeIds: new Set(), anchor: null }
}

/**
 * 行が選択集合に入っているか。filter 形は「除外されていない」で近似する
 * （表示フィルタが選択時と違うときは表示中の行が集合に入っているかを client では判定できない。
 * 呼び出し側は `displayFilter === sel.filter` のときだけハイライトに使う。D-40）
 */
export function isSelected(sel: Selection, id: number): boolean {
  switch (sel.kind) {
    case 'none':
      return false
    case 'ids':
      return sel.ids.has(id)
    case 'filter':
      return !sel.excludeIds.has(id)
  }
}

/** POST /api/tracks/batch/preview の selection（SPEC §9） */
export function toSelectionBody(
  sel: Selection,
): { ids: number[] } | { filter: string; exclude_ids: number[] } | null {
  switch (sel.kind) {
    case 'none':
      return null
    case 'ids':
      return { ids: [...sel.ids].sort((a, b) => a - b) }
    case 'filter':
      return { filter: sel.filter, exclude_ids: [...sel.excludeIds].sort((a, b) => a - b) }
  }
}

/**
 * Ctrl+A の時点で 1 ページ目がまだ届いておらず total が無かったとき、同じフィルタの total が
 * 届いたらそれを選択時の値として一度だけ確定する。既に値があれば動かさない（immutable）
 */
export function settleFilterTotal(
  sel: Selection,
  selectionTotal: number | null,
  displayFilter: string,
  displayTotal: number | null,
): number | null {
  if (selectionTotal != null) return selectionTotal
  if (sel.kind !== 'filter' || displayTotal == null) return null
  return sel.filter === displayFilter ? displayTotal : null
}

/** 件数。filter 形はサーバの total（選択時のフィルタで数えたもの）から除外を引く */
export function selectionCount(sel: Selection, filterTotal: number | null): number | null {
  switch (sel.kind) {
    case 'none':
      return 0
    case 'ids':
      return sel.ids.size
    case 'filter':
      return filterTotal == null ? null : Math.max(0, filterTotal - sel.excludeIds.size)
  }
}
