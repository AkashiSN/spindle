import { describe, expect, it } from 'vitest'
import {
  clickRow,
  isSelected,
  NO_SELECTION,
  selectAll,
  selectionCount,
  settleFilterTotal,
  toSelectionBody,
  type Selection,
} from './selection'

const order = [10, 20, 30, 40, 50]
const plain = { shift: false, ctrl: false }
const shift = { shift: true, ctrl: false }
const ctrl = { shift: false, ctrl: true }

function ids(sel: Selection): number[] {
  const b = toSelectionBody(sel)
  if (b && 'ids' in b) return b.ids
  throw new Error(`ids 形でない: ${JSON.stringify(b)}`)
}

describe('clickRow', () => {
  it('素のクリックは 1 件に置き換える', () => {
    let s = clickRow(NO_SELECTION, 20, plain, order)
    expect(ids(s)).toEqual([20])
    s = clickRow(s, 40, plain, order)
    expect(ids(s)).toEqual([40])
  })

  it('Shift は anchor からの範囲を足す（逆方向も）', () => {
    let s = clickRow(NO_SELECTION, 20, plain, order)
    s = clickRow(s, 40, shift, order)
    expect(ids(s)).toEqual([20, 30, 40])
    s = clickRow(s, 10, shift, order)
    expect(ids(s)).toEqual([10, 20, 30, 40]) // anchor は動かない
  })

  it('Ctrl はトグル。空になったら none', () => {
    let s = clickRow(NO_SELECTION, 20, plain, order)
    s = clickRow(s, 40, ctrl, order)
    expect(ids(s)).toEqual([20, 40])
    s = clickRow(s, 20, ctrl, order)
    expect(ids(s)).toEqual([40])
    s = clickRow(s, 40, ctrl, order)
    expect(s).toEqual(NO_SELECTION)
  })

  it('anchor がソート・フィルタで表示から消えていたら Shift は素のクリック扱い', () => {
    let s = clickRow(NO_SELECTION, 20, plain, order)
    s = clickRow(s, 40, shift, [30, 40, 50])
    expect(ids(s)).toEqual([40])
  })

  it('選択は表示の変化で変わらない（immutable）', () => {
    const s = clickRow(clickRow(NO_SELECTION, 20, plain, order), 40, shift, order)
    const before = ids(s)
    // 別の順序・別の集合で描き直しても値は同じオブジェクト
    expect(ids(s)).toEqual(before)
    expect(isSelected(s, 30)).toBe(true)
    expect(isSelected(s, 50)).toBe(false)
  })
})

describe('filter 形', () => {
  it('Ctrl+A はフィルタ式を固定し、Ctrl クリックで除外・Shift で戻す', () => {
    let s = selectAll('{"category":"J-Pop"}')
    expect(toSelectionBody(s)).toEqual({ filter: '{"category":"J-Pop"}', exclude_ids: [] })
    expect(selectionCount(s, 1207)).toBe(1207)
    expect(selectionCount(s, null)).toBeNull()

    s = clickRow(s, 30, ctrl, order)
    s = clickRow(s, 50, ctrl, order)
    expect(toSelectionBody(s)).toEqual({ filter: '{"category":"J-Pop"}', exclude_ids: [30, 50] })
    expect(selectionCount(s, 1207)).toBe(1205)
    expect(isSelected(s, 30)).toBe(false)
    expect(isSelected(s, 40)).toBe(true)

    // Shift で範囲を選び直す（除外を解除）
    s = clickRow(s, 30, shift, order) // anchor=50 → 30..50
    expect(toSelectionBody(s)).toEqual({ filter: '{"category":"J-Pop"}', exclude_ids: [] })
    // Ctrl で除外し直しても filter 文字列は変わらない
    s = clickRow(s, 10, ctrl, order)
    expect(s.kind === 'filter' && s.filter).toBe('{"category":"J-Pop"}')
  })

  it('表示フィルタが選択時と違う間は Ctrl / Shift を無視し、素のクリックだけ効く', () => {
    const sel = clickRow(selectAll('{"category":"J-Pop"}'), 30, ctrl, order, '{"category":"J-Pop"}')
    expect(selectionCount(sel, 100)).toBe(99)
    // 別の表示フィルタ上で Ctrl / Shift → 集合は変わらない
    const same = clickRow(sel, 40, ctrl, order, '{"flags":["missing"]}')
    expect(same).toBe(sel)
    expect(clickRow(sel, 50, shift, order, '{"flags":["missing"]}')).toBe(sel)
    expect(selectionCount(same, 100)).toBe(99)
    // 同じ表示フィルタに戻れば操作できる
    const back = clickRow(sel, 40, ctrl, order, '{"category":"J-Pop"}')
    expect(selectionCount(back, 100)).toBe(98)
    // 素のクリックは ids 形へ
    expect(ids(clickRow(sel, 40, plain, order, '{"flags":["missing"]}'))).toEqual([40])
    // displayFilter を渡さない呼び出し（テスト等）は従来どおり
    expect(selectionCount(clickRow(sel, 40, ctrl, order), 100)).toBe(98)
  })

  it('Ctrl+A 時に total が無ければ、同じフィルタの total が届いたときに一度だけ確定する', () => {
    const sel = selectAll('{}')
    expect(settleFilterTotal(sel, null, '{}', null)).toBeNull()
    expect(settleFilterTotal(sel, null, '{"q":"x"}', 5)).toBeNull() // 別フィルタの total は使わない
    expect(settleFilterTotal(sel, null, '{}', 60000)).toBe(60000)
    expect(settleFilterTotal(sel, 60000, '{}', 60010)).toBe(60000) // 一度決まったら動かない
    expect(settleFilterTotal(NO_SELECTION, null, '{}', 5)).toBeNull()
  })

  it('素のクリックは filter 形を捨てて ids 形にする', () => {
    const s = clickRow(selectAll(''), 30, plain, order)
    expect(ids(s)).toEqual([30])
  })
})
