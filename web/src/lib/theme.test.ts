import { afterEach, beforeEach, describe, expect, it } from 'vitest'
import rawCss from '../index.css?raw'
import { isThemePref, loadThemePref, resolveTheme, saveThemePref } from './theme'

// node 環境には localStorage が無いので最小のスタブを置く
function stubStorage(initial: Record<string, string> = {}): Record<string, string> {
  const store = { ...initial }
  const ls = {
    getItem: (k: string) => (k in store ? store[k]! : null),
    setItem: (k: string, v: string) => {
      store[k] = v
    },
    removeItem: (k: string) => {
      delete store[k]
    },
  }
  ;(globalThis as { localStorage?: unknown }).localStorage = ls
  return store
}

describe('resolveTheme', () => {
  it('保存値が light / dark ならそれを採り、system なら OS に従う', () => {
    expect(resolveTheme('light', true)).toBe('light')
    expect(resolveTheme('dark', false)).toBe('dark')
    expect(resolveTheme('system', true)).toBe('dark')
    expect(resolveTheme('system', false)).toBe('light')
  })
})

describe('isThemePref', () => {
  it('3 値だけを受け付ける', () => {
    expect(isThemePref('light')).toBe(true)
    expect(isThemePref('dark')).toBe(true)
    expect(isThemePref('system')).toBe(true)
    expect(isThemePref('auto')).toBe(false)
    expect(isThemePref(1)).toBe(false)
    expect(isThemePref(null)).toBe(false)
  })
})

describe('loadThemePref / saveThemePref', () => {
  let store: Record<string, string>
  beforeEach(() => {
    store = stubStorage()
  })
  afterEach(() => {
    delete (globalThis as { localStorage?: unknown }).localStorage
  })

  it('未保存なら system', () => {
    expect(loadThemePref()).toBe('system')
  })

  it('保存した値を読み戻す（キーは spindle:theme）', () => {
    saveThemePref('dark')
    expect(store['spindle:theme']).toBe('"dark"')
    expect(loadThemePref()).toBe('dark')
  })

  it('壊れた保存値は system に倒す', () => {
    store['spindle:theme'] = '"purple"'
    expect(loadThemePref()).toBe('system')
    store['spindle:theme'] = '{'
    expect(loadThemePref()).toBe('system')
  })
})

describe('index.css の配色', () => {
  // 直書きの色（#hex / rgb() / rgba()）は変数の定義ブロック（:root と :root[data-theme='dark']）に
  // だけ置く。それ以外にあるとダークで追随しない
  it('直書きの色は :root の 2 ブロック以外に無い', () => {
    // コメントを落としてから { } で区切る（ネスト無し。@media は使っていない）
    const css = rawCss.replace(/\/\*[\s\S]*?\*\//g, '')
    const blocks: { header: string; body: string }[] = []
    const re = /([^{}]+)\{([^{}]*)\}/g
    for (let m = re.exec(css); m != null; m = re.exec(css)) {
      blocks.push({ header: m[1]!.trim(), body: m[2]! })
    }
    // 色の関数（rgb / hsl / oklch …）と名前付きの色も直書きとみなす（transparent / inherit / currentColor は色ではない）
    const colour = /#[0-9a-fA-F]{3,8}\b|\b(?:rgba?|hsla?|oklch|oklab|lab|lch|color)\(|(?::|\s)(?:white|black|red|green|blue|gray|grey|orange|yellow|silver|navy|teal|purple|pink)(?=[\s;,)])/
    const isVarBlock = (h: string) => h === ':root' || h === ":root[data-theme='dark']"
    const offenders = blocks.filter((b) => !isVarBlock(b.header) && colour.test(b.body)).map((b) => b.header)
    expect(offenders).toEqual([])
    // 変数ブロックは両方ある
    expect(blocks.some((b) => b.header === ':root')).toBe(true)
    expect(blocks.some((b) => b.header === ":root[data-theme='dark']")).toBe(true)
  })

  it('ライトで定義した色の変数はダークでも全部定義する', () => {
    const css = rawCss
    const vars = (header: string) => {
      const i = css.indexOf(header + ' {')
      expect(i, header).toBeGreaterThanOrEqual(0)
      const body = css.slice(i, css.indexOf('}', i))
      return new Set([...body.matchAll(/(--[a-z0-9-]+):\s*(#|rgba?\(|hsla?\(|oklch\()/g)].map((m) => m[1]!))
    }
    const light = vars(':root')
    const dark = vars(":root[data-theme='dark']")
    expect([...light].filter((v) => !dark.has(v))).toEqual([])
    expect([...dark].filter((v) => !light.has(v))).toEqual([])
  })
})
