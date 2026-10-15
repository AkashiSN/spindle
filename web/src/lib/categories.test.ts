import { describe, expect, it, test, vi } from 'vitest'
import {
  categoryUsageText,
  deleteConfirmText,
  notifyCategoriesChanged,
  onCategoriesChanged,
  otherUsageText,
} from './categories'

describe('categories', () => {
  it('取り直しの合図は購読中にだけ届き、解除した後は届かない', () => {
    const a = vi.fn()
    const b = vi.fn()
    const offA = onCategoriesChanged(a)
    const offB = onCategoriesChanged(b)
    notifyCategoriesChanged()
    expect(a).toHaveBeenCalledTimes(1)
    expect(b).toHaveBeenCalledTimes(1)
    offA()
    notifyCategoriesChanged()
    expect(a).toHaveBeenCalledTimes(1)
    expect(b).toHaveBeenCalledTimes(2)
    offB()
  })
  it('購読者の中で解除しても残りに届く', () => {
    const calls: string[] = []
    const off1 = onCategoriesChanged(() => {
      calls.push('1')
      off1()
    })
    const off2 = onCategoriesChanged(() => calls.push('2'))
    notifyCategoriesChanged()
    expect(calls).toEqual(['1', '2'])
    off2()
  })
  it('削除の確認文に名前と再作成の条件が入る', () => {
    const t = deleteConfirmText('Temp')
    expect(t).toContain('「Temp」')
    expect(t).toContain('次のスキャン')
  })
})

test('使用中の理由の文言（未使用は null）', () => {
  expect(categoryUsageText({ id: 1, name: 'Anime', albums: 528, genres: 0, subscriptions: 0, inbox_drafts: 0 })).toBe(
    '528 album で使用中',
  )
  expect(categoryUsageText({ id: 2, name: 'J-Pop', albums: 0, genres: 1, subscriptions: 2, inbox_drafts: 0 })).toBe(
    'GENRE の写像 1・購読 2 で使用中',
  )
  expect(categoryUsageText({ id: 3, name: 'Empty', albums: 0, genres: 0, subscriptions: 0, inbox_drafts: 0 })).toBeNull()
  expect(categoryUsageText({ id: 4, name: 'New' })).toBeNull()
})

test('その他の使用の列', () => {
  expect(otherUsageText({ id: 1, name: 'x', albums: 5, genres: 0, subscriptions: 1, inbox_drafts: 2 })).toBe(
    '購読 1・Inbox の下書き 2',
  )
  expect(otherUsageText({ id: 1, name: 'x', albums: 5 })).toBe('')
})
