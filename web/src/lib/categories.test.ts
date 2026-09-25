import { describe, expect, it, vi } from 'vitest'
import { deleteConfirmText, notifyCategoriesChanged, onCategoriesChanged } from './categories'

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
