// スマホ版（/m/）の骨格。ログイン → タブの中身 + 下タブ。
// - 認証: getSession() が 401 ならログイン画面。API の 401（onUnauthorized）でも戻す
// - 位置は URL（route.ts）。タブを切り替えるたびに履歴を積むので、戻るで前のタブに戻る（今と同じ URL なら積まない）
// - SSE は EventsProvider が 1 本だけ開く
// - 曲のシートは history.state（{ sheet: trackId, edit?: key }）で持つ。開くと履歴を積み、戻るで閉じる
//   （UI の「閉じる」も history.back()）。試し聴きの <audio> は Shell に 1 つ

import { useCallback, useEffect, useState } from 'react'
import { ApiError, onUnauthorized } from '../api/client'
import type { TrackRow } from '../api/types'
import { getSession } from './api'
import { FolderTab } from './components/FolderTab'
import { LibraryTab } from './components/LibraryTab'
import { Login } from './components/Login'
import { MiniPlayer } from './components/MiniPlayer'
import { TabBar } from './components/TabBar'
import { TrackSheet } from './components/TrackSheet'
import { TABS, useRoute, type Route, type Tab } from './route'
import { loadLibraryView } from './library'
import { sheetFromHistory, type SheetState } from './track'
import { usePreviewPlayer } from './usePreviewPlayer'
import { EventsProvider } from './useEvents'
import { pcPreferenceCookie } from './view'

type Auth = 'checking' | 'login' | 'ok' | 'error'

/** タブを選んだときの行き先（そのタブの最上段） */
function tabRoot(tab: Tab): Route {
  switch (tab) {
    case 'library':
      return { tab: 'library', view: loadLibraryView() }
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

/** 開いている曲のシート（history.state と同期する） */
function useSheet() {
  const [sheet, setSheet] = useState<SheetState | null>(() => sheetFromHistory(window.history.state))
  useEffect(() => {
    const onPop = () => setSheet(sheetFromHistory(window.history.state))
    window.addEventListener('popstate', onPop)
    return () => window.removeEventListener('popstate', onPop)
  }, [])
  /** 曲のシートを開く。既に開いていれば積まずに置き換える */
  const open = useCallback((trackId: number) => {
    const state = { sheet: trackId }
    if (sheetFromHistory(window.history.state)) window.history.replaceState(state, '', window.location.href)
    else window.history.pushState(state, '', window.location.href)
    setSheet({ trackId, editKey: null })
  }, [])
  /** 修正シートを開く（戻るで修正シートだけ閉じる） */
  const openEdit = useCallback((key: string) => {
    const cur = sheetFromHistory(window.history.state)
    if (!cur || cur.editKey != null) return
    window.history.pushState({ sheet: cur.trackId, edit: key }, '', window.location.href)
    setSheet({ trackId: cur.trackId, editKey: key })
  }, [])
  /** いちばん上のシートを閉じる（開いたときに積んだ履歴を戻る） */
  const back = useCallback(() => window.history.back(), [])
  /** navigate の後に呼ぶ（積んだ画面ではシートは閉じている） */
  const sync = useCallback(() => setSheet(sheetFromHistory(window.history.state)), [])
  return { sheet, open, openEdit, back, sync }
}

function Shell() {
  const [route, routeNavigate] = useRoute()
  const [menuOpen, setMenuOpen] = useState(false)
  const { sheet, open, openEdit, back, sync } = useSheet()
  const navigate = useCallback(
    (r: Route, opts?: { replace?: boolean }) => {
      routeNavigate(r, opts)
      sync()
    },
    [routeNavigate, sync],
  )
  // 一覧で選んだ行（詳細が届くまでシートの見出しに使う）
  const [picked, setPicked] = useState<TrackRow | null>(null)
  const setOpenTrack = useCallback(
    (row: TrackRow) => {
      setPicked(row)
      open(row.id)
    },
    [open],
  )
  const player = usePreviewPlayer()
  const title = TABS.find(([t]) => t === route.tab)?.[1] ?? ''

  let content
  switch (route.tab) {
    case 'library':
      content = <LibraryTab route={route} navigate={navigate} onOpenTrack={setOpenTrack} />
      break
    case 'folders':
      content = <FolderTab route={route} navigate={navigate} onOpenTrack={setOpenTrack} />
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
      <MiniPlayer player={player} onOpen={setOpenTrack} />
      <TabBar current={route.tab} onSelect={(t) => navigate(tabRoot(t))} />
      {sheet && (
        <TrackSheet
          key={sheet.trackId}
          trackId={sheet.trackId}
          initial={picked}
          editKey={sheet.editKey}
          player={player}
          onClose={back}
          onEdit={openEdit}
          onEditClose={back}
        />
      )}
    </div>
  )
}

/** このタスクでは各タブは空。後続のタスクで LibraryTab などに置き換える */
function EmptyTab({ title }: { title: string }) {
  return <p className="m-placeholder">{title}</p>
}
