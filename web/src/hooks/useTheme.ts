// 配色テーマ（SPEC §12.6「表示」、D-58 追記）。設定は localStorage に持ち、'system' のときは
// OS の prefers-color-scheme の変化にも追随する。解決結果は <html data-theme> に書き、
// index.css の :root[data-theme='dark'] が変数を差し替える

import { useEffect, useState } from 'react'
import { isThemePref, resolveTheme, THEME_KEY, type Theme, type ThemePref } from '../lib/theme'
import { useLocalStorageState } from './useLocalStorageState'

const QUERY = '(prefers-color-scheme: dark)'

function osDark(): boolean {
  return typeof window !== 'undefined' && typeof window.matchMedia === 'function' && window.matchMedia(QUERY).matches
}

export interface ThemeState {
  pref: ThemePref
  theme: Theme
  setPref: (pref: ThemePref) => void
}

export function useTheme(): ThemeState {
  const [pref, setPref] = useLocalStorageState<ThemePref>(THEME_KEY, 'system', isThemePref)
  const [dark, setDark] = useState(osDark)
  useEffect(() => {
    if (typeof window.matchMedia !== 'function') return
    const mq = window.matchMedia(QUERY)
    const onChange = (e: MediaQueryListEvent) => setDark(e.matches)
    mq.addEventListener('change', onChange)
    return () => mq.removeEventListener('change', onChange)
  }, [])
  const theme = resolveTheme(pref, dark)
  useEffect(() => {
    document.documentElement.dataset.theme = theme
  }, [theme])
  return { pref, theme, setPref }
}
