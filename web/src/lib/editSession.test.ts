import { describe, expect, it } from 'vitest'
import { editSession } from './editSession'

describe('editSession', () => {
  it('Esc の後の blur は確定しない（取り消した値が残らない）', () => {
    const s = editSession()
    s.start()
    expect(s.key('Escape')).toBe('cancel')
    expect(s.blur()).toBeNull()
  })
  it('Enter の後の blur で二重に確定しない', () => {
    const s = editSession()
    s.start()
    expect(s.key('Enter')).toBe('commit')
    expect(s.blur()).toBeNull()
  })
  it('blur だけなら確定し、その後のキーでは何もしない', () => {
    const s = editSession()
    s.start()
    expect(s.key('a')).toBeNull()
    expect(s.blur()).toBe('commit')
    expect(s.key('Enter')).toBeNull()
  })
  it('次の編集を始めると決着を忘れる', () => {
    const s = editSession()
    s.start()
    s.key('Escape')
    s.start()
    expect(s.blur()).toBe('commit')
  })
})
