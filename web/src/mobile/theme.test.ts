import { describe, expect, it } from 'vitest'
import pcCss from '../index.css?raw'
import mobileCss from './mobile.css?raw'

// PC の index.css とスマホ版の mobile.css で配色の変数が同じ値かを確かめる
function vars(css: string, selector: string): Map<string, string> {
  const start = css.indexOf(`${selector} {`)
  if (start < 0) throw new Error(`${selector} が無い`)
  const body = css.slice(start, css.indexOf('\n}', start))
  const out = new Map<string, string>()
  for (const m of body.matchAll(/^\s*(--[\w-]+):\s*([^;]+);/gm)) out.set(m[1]!, m[2]!.trim())
  return out
}

// PC のレイアウト用の変数（配色ではない）
const LAYOUT = new Set(['--sidebar-w', '--props-h', '--art-h', '--bar-h', '--row-h'])

describe('mobile.css の配色', () => {
  for (const sel of [':root', ":root[data-theme='dark']"]) {
    it(`${sel} の配色が PC と同じ`, () => {
      const pc = vars(pcCss, sel)
      const m = vars(mobileCss, sel)
      const colors = [...pc].filter(([k]) => !LAYOUT.has(k))
      expect(colors.length).toBeGreaterThan(10)
      for (const [k, v] of colors) expect([k, m.get(k)]).toEqual([k, v])
    })
  }
})
