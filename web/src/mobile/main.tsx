// スマホ版（/m/）の入口

import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import { loadThemePref, resolveTheme } from '../lib/theme'
import { App } from './App'
import { clearPcPreference } from './view'
import './mobile.css'

// スマホ版を開いたら「PC 版を開く」の指定を消す
clearPcPreference(document)

// テーマは PC と同じ保存値（lib/theme.ts）。'system' なら OS の切り替えにも追従する
const media = window.matchMedia('(prefers-color-scheme: dark)')
function applyTheme() {
  document.documentElement.dataset.theme = resolveTheme(loadThemePref(), media.matches)
}
applyTheme()
media.addEventListener('change', applyTheme)

const root = document.getElementById('root')
if (root)
  createRoot(root).render(
    <StrictMode>
      <App />
    </StrictMode>,
  )
