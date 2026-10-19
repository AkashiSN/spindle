// スマホ版（/m/）の画面の位置を URL で持つ。ブラウザの戻るで前の画面に戻れるように
// history.pushState と popstate で同期する。
//
// 形:
//   /m/library/{artists|albums|tracks}?albumartist=..&album=..&q=..
//   /m/folders/{category}/{albumartist}/{album}   （各段は encodeURIComponent）
//   /m/playlists/{id}?edit={rule|text}
//   /m/subscriptions/{id}
// 曲のシートは route ではなく history.state（{ sheet: trackId }）で持ち、戻るで閉じる

import { useCallback, useEffect, useState } from 'react'

export type Tab = 'library' | 'folders' | 'playlists' | 'subscriptions'
export type LibraryView = 'artists' | 'albums' | 'tracks'
export type Route =
  | { tab: 'library'; view: LibraryView; albumartist?: string; albumId?: number; q?: string }
  | { tab: 'folders'; path: string[] } // [category, albumartist, album] の途中まで
  | { tab: 'playlists'; playlistId?: number; edit?: 'rule' | 'text' }
  | { tab: 'subscriptions'; subscriptionId?: number }

export const BASE = '/m'

/** 下タブの並びと表示名 */
export const TABS: readonly [Tab, string][] = [
  ['library', 'ライブラリ'],
  ['folders', 'フォルダ'],
  ['playlists', 'プレイリスト'],
  ['subscriptions', '購読'],
]

function isView(v: string | undefined): v is LibraryView {
  return v === 'artists' || v === 'albums' || v === 'tracks'
}

/** 正の整数だけを受け付ける（それ以外は undefined） */
function parseId(v: string | null | undefined): number | undefined {
  if (v == null || !/^[1-9][0-9]*$/.test(v)) return undefined
  const n = Number(v)
  return Number.isSafeInteger(n) ? n : undefined
}

function decode(seg: string): string | null {
  try {
    return decodeURIComponent(seg)
  } catch {
    return null
  }
}

export function parseRoute(pathname: string, search: string): Route {
  const rest = pathname.startsWith(`${BASE}/`) ? pathname.slice(BASE.length + 1) : pathname === BASE ? '' : null
  const segs = (rest ?? '').split('/').filter((s) => s !== '')
  const q = new URLSearchParams(search)
  switch (segs[0]) {
    case 'folders': {
      const path: string[] = []
      for (const s of segs.slice(1, 4)) {
        const d = decode(s)
        if (d == null) break
        path.push(d)
      }
      return { tab: 'folders', path }
    }
    case 'playlists': {
      const r: Route = { tab: 'playlists' }
      const id = parseId(segs[1])
      if (id != null) {
        r.playlistId = id
        const edit = q.get('edit')
        if (edit === 'rule' || edit === 'text') r.edit = edit
      }
      return r
    }
    case 'subscriptions': {
      const r: Route = { tab: 'subscriptions' }
      const id = parseId(segs[1])
      if (id != null) r.subscriptionId = id
      return r
    }
    case 'library': {
      const r: Route = { tab: 'library', view: isView(segs[1]) ? segs[1] : 'albums' }
      const aa = q.get('albumartist')
      if (aa != null) r.albumartist = aa
      const albumId = parseId(q.get('album'))
      if (albumId != null) r.albumId = albumId
      const text = q.get('q')
      if (text != null && text !== '') r.q = text
      return r
    }
    default:
      return { tab: 'library', view: 'albums' }
  }
}

export function formatRoute(r: Route): string {
  switch (r.tab) {
    case 'library': {
      const p = new URLSearchParams()
      if (r.albumartist != null) p.set('albumartist', r.albumartist)
      if (r.albumId != null) p.set('album', String(r.albumId))
      if (r.q != null && r.q !== '') p.set('q', r.q)
      const s = p.toString()
      return `${BASE}/library/${r.view}${s ? `?${s}` : ''}`
    }
    case 'folders':
      return [`${BASE}/folders`, ...r.path.slice(0, 3).map(encodeURIComponent)].join('/')
    case 'playlists':
      if (r.playlistId == null) return `${BASE}/playlists`
      return `${BASE}/playlists/${r.playlistId}${r.edit ? `?edit=${r.edit}` : ''}`
    case 'subscriptions':
      return r.subscriptionId == null ? `${BASE}/subscriptions` : `${BASE}/subscriptions/${r.subscriptionId}`
  }
}

function current(): Route {
  return parseRoute(window.location.pathname, window.location.search)
}

/** 現在の route と、移動する関数。移動は pushState（`replace` なら replaceState） */
export function useRoute(): [Route, (r: Route, opts?: { replace?: boolean }) => void] {
  const [route, setRoute] = useState<Route>(current)
  useEffect(() => {
    const onPop = () => setRoute(current())
    window.addEventListener('popstate', onPop)
    return () => window.removeEventListener('popstate', onPop)
  }, [])
  const navigate = useCallback((r: Route, opts?: { replace?: boolean }) => {
    const url = formatRoute(r)
    if (opts?.replace) window.history.replaceState(null, '', url)
    else window.history.pushState(null, '', url)
    setRoute(r)
  }, [])
  return [route, navigate]
}
