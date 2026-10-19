import { describe, expect, it } from 'vitest'
import { clearPcPreference, pcPreferenceCookie, VIEW_COOKIE } from './view'

describe('view', () => {
  it('PC 版の Cookie を立てて消せる', () => {
    expect(VIEW_COOKIE).toBe('spindle_view')
    expect(pcPreferenceCookie()).toBe('spindle_view=pc; path=/; max-age=31536000; samesite=lax')
    const doc = { cookie: '' }
    clearPcPreference(doc)
    expect(doc.cookie).toBe('spindle_view=; path=/; max-age=0; samesite=lax')
  })
})
