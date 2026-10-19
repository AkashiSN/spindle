// スマホ版（/m/）の骨格。ログイン → タブの中身 + 下タブ。
// - 認証: getSession() が 401 ならログイン画面。API の 401（onUnauthorized）でも戻す
// - 位置は URL（route.ts）。タブを切り替えるたびに履歴を積むので、戻るで前のタブに戻る
// - SSE は EventsProvider が 1 本だけ開く

import { useEffect, useState } from 'react'
import { ApiError, onUnauthorized } from '../api/client'
import { getSession } from './api'
import { Login } from './components/Login'
import { TabBar } from './components/TabBar'
import { TABS, useRoute, type Route, type Tab } from './route'
import { EventsProvider } from './useEvents'
import { pcPreferenceCookie } from './view'

type Auth = 'checking' | 'login' | 'ok' | 'error'

/** タブを選んだときの行き先（そのタブの最上段） */
function tabRoot(tab: Tab): Route {
  switch (tab) {
    case 'library':
      return { tab: 'library', view: 'albums' }
    case 'folders':
      return { tab: 'folders', path: [] }
    case 'playlists':
      return { tab: 'playlists' }
    case 'subscriptions':
      return { tab: 'subscriptions' }
  }
}

function openPc() {
  document.cookie = pcPreferenceCookie()
  window.location.href = '/'
}

export function App() {
  const [auth, setAuth] = useState<Auth>('checking')

  useEffect(() => {
    let alive = true
    getSession()
      .then(() => {
        if (alive) setAuth('ok')
      })
      .catch((e: unknown) => {
        if (!alive) return
        setAuth(e instanceof ApiError && e.status === 401 ? 'login' : 'error')
      })
    const off = onUnauthorized(() => setAuth('login'))
    return () => {
      alive = false
      off()
    }
  }, [])

  if (auth === 'checking') return <main className="m-center">読み込み中…</main>
  if (auth === 'error')
    return (
      <main className="m-center">
        <p className="m-error">サーバに接続できません</p>
        <button type="button" onClick={() => window.location.reload()}>
          再読み込み
        </button>
      </main>
    )
  if (auth === 'login') return <Login onLoggedIn={() => setAuth('ok')} />
  return (
    <EventsProvider enabled>
      <Shell />
    </EventsProvider>
  )
}

function Shell() {
  const [route, navigate] = useRoute()
  const [menuOpen, setMenuOpen] = useState(false)
  const title = TABS.find(([t]) => t === route.tab)?.[1] ?? ''

  let content
  switch (route.tab) {
    case 'library':
      content = <EmptyTab title="ライブラリ" />
      break
    case 'folders':
      content = <EmptyTab title="フォルダ" />
      break
    case 'playlists':
      content = <EmptyTab title="プレイリスト" />
      break
    case 'subscriptions':
      content = <EmptyTab title="購読" />
      break
  }

  return (
    <div className="m-app">
      <header className="m-header">
        <h1>{title}</h1>
        <button type="button" className="m-icon" aria-label="メニュー" aria-expanded={menuOpen} onClick={() => setMenuOpen((v) => !v)}>
          ⋯
        </button>
        {menuOpen && (
          <>
            <div className="m-backdrop" onClick={() => setMenuOpen(false)} />
            <div className="m-menu" role="menu">
              <button type="button" role="menuitem" onClick={openPc}>
                PC 版を開く
              </button>
            </div>
          </>
        )}
      </header>
      <main className="m-content">{content}</main>
      <TabBar current={route.tab} onSelect={(t) => navigate(tabRoot(t))} />
    </div>
  )
}

/** このタスクでは各タブは空。後続のタスクで LibraryTab などに置き換える */
function EmptyTab({ title }: { title: string }) {
  return <p className="m-placeholder">{title}</p>
}
