import { describe, expect, it } from 'vitest'
import { asNavKey, isInteractiveTarget, moveCursor, resolveCursor } from './keynav'

describe('asNavKey', () => {
  it('矢印・Page・Home・End だけを移動キーとして扱う', () => {
    expect(asNavKey('ArrowDown')).toBe('ArrowDown')
    expect(asNavKey('ArrowUp')).toBe('ArrowUp')
    expect(asNavKey('PageDown')).toBe('PageDown')
    expect(asNavKey('PageUp')).toBe('PageUp')
    expect(asNavKey('Home')).toBe('Home')
    expect(asNavKey('End')).toBe('End')
    expect(asNavKey('ArrowLeft')).toBeNull()
    expect(asNavKey('a')).toBeNull()
    expect(asNavKey('Enter')).toBeNull()
  })
})

describe('moveCursor', () => {
  const loaded = 10
  const page = 4

  it('読み込み済みの行が無ければ動かない', () => {
    expect(moveCursor('ArrowDown', null, 0, page)).toBeNull()
    expect(moveCursor('End', 3, 0, page)).toBeNull()
  })

  it('カーソルが無いとき、下向きは先頭、上向きは読み込み済みの末尾から始める', () => {
    expect(moveCursor('ArrowDown', null, loaded, page)).toBe(0)
    expect(moveCursor('PageDown', null, loaded, page)).toBe(0)
    expect(moveCursor('Home', null, loaded, page)).toBe(0)
    expect(moveCursor('ArrowUp', null, loaded, page)).toBe(9)
    expect(moveCursor('PageUp', null, loaded, page)).toBe(9)
    expect(moveCursor('End', null, loaded, page)).toBe(9)
  })

  it('矢印は 1 行、Page は 1 画面ぶん動き、端で止まる', () => {
    expect(moveCursor('ArrowDown', 3, loaded, page)).toBe(4)
    expect(moveCursor('ArrowUp', 3, loaded, page)).toBe(2)
    expect(moveCursor('ArrowUp', 0, loaded, page)).toBe(0)
    expect(moveCursor('ArrowDown', 9, loaded, page)).toBe(9)
    expect(moveCursor('PageDown', 3, loaded, page)).toBe(7)
    expect(moveCursor('PageDown', 8, loaded, page)).toBe(9)
    expect(moveCursor('PageUp', 5, loaded, page)).toBe(1)
    expect(moveCursor('PageUp', 2, loaded, page)).toBe(0)
  })

  it('Home / End は読み込み済みの範囲の両端', () => {
    expect(moveCursor('Home', 5, loaded, page)).toBe(0)
    expect(moveCursor('End', 5, loaded, page)).toBe(9)
  })

  it('読み込み済みの外（未読込の骨組み行）へは出ない', () => {
    expect(moveCursor('ArrowDown', 9, loaded, page)).toBe(9)
    expect(moveCursor('End', 0, loaded, page)).toBe(9)
    // カーソルが読み込み済みの外を指していたら（表示の入れ替わり）末尾に戻す
    expect(moveCursor('ArrowDown', 20, loaded, page)).toBe(9)
  })

  it('画面の行数が 0 以下でも Page は最低 1 行動く', () => {
    expect(moveCursor('PageDown', 3, loaded, 0)).toBe(4)
    expect(moveCursor('PageUp', 3, loaded, -1)).toBe(2)
  })
})

describe('resolveCursor', () => {
  it('読み込み済みの行に居るカーソルだけ有効。消えていれば null', () => {
    expect(resolveCursor(30, [10, 20, 30])).toBe(30)
    expect(resolveCursor(40, [10, 20, 30])).toBeNull()
    expect(resolveCursor(null, [10, 20, 30])).toBeNull()
    expect(resolveCursor(10, [])).toBeNull()
  })
})

describe('isInteractiveTarget', () => {
  it('input / button / select / textarea / contenteditable からのキーは一覧の操作にしない', () => {
    for (const tag of ['INPUT', 'BUTTON', 'SELECT', 'TEXTAREA']) {
      expect(isInteractiveTarget({ tagName: tag, isContentEditable: false })).toBe(true)
    }
    expect(isInteractiveTarget({ tagName: 'DIV', isContentEditable: true })).toBe(true)
    expect(isInteractiveTarget({ tagName: 'DIV', isContentEditable: false })).toBe(false)
    expect(isInteractiveTarget(null)).toBe(false)
  })
})
