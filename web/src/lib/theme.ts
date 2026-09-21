// 配色テーマ（SPEC §12.6「表示」、D-58 追記）。設定は localStorage の `spindle:theme` に
// 'light' / 'dark' / 'system' で置き、'system' は OS（prefers-color-scheme）に従う。
// 解決結果は <html data-theme="..."> に書き、index.css は `:root[data-theme='dark']` で
// 変数を差し替える。index.html のインラインスクリプトも同じ規則で初期値を付ける（白飛び防止）

import { loadJson, saveJson } from './storage'

export type ThemePref = 'light' | 'dark' | 'system'
export type Theme = 'light' | 'dark'

export const THEME_KEY = 'theme'

export const THEME_PREFS: readonly [ThemePref, string][] = [
  ['system', 'OS に従う'],
  ['light', 'ライト'],
  ['dark', 'ダーク'],
]

export function isThemePref(v: unknown): v is ThemePref {
  return v === 'light' || v === 'dark' || v === 'system'
}

/** 保存値 > OS。'system' のときだけ OS のダーク設定を見る */
export function resolveTheme(pref: ThemePref, osDark: boolean): Theme {
  if (pref === 'system') return osDark ? 'dark' : 'light'
  return pref
}

export function loadThemePref(): ThemePref {
  return loadJson<ThemePref>(THEME_KEY, 'system', isThemePref)
}

export function saveThemePref(pref: ThemePref): void {
  saveJson(THEME_KEY, pref)
}
