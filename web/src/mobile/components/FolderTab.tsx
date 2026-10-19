// フォルダのタブ。カテゴリ → アルバムアーティスト → アルバム と 1 段ずつ降りる（位置は route の path）。
// 上にパンくず（各段をタップで戻る）、album の段では曲の一覧

import { useCallback, useEffect, useMemo, useState } from 'react'
import { ApiError } from '../../api/client'
import type { AlbumRow, TrackRow } from '../../api/types'
import { TrailingDebounce } from '../../lib/debounce'
import type { Filter, Sort } from '../../lib/filter'
import { formatCount } from '../../lib/format'
import { buildTree } from '../../lib/tree'
import { listAlbums } from '../api'
import { FOLDER_PATTERN, folderLevel } from '../folders'
import type { Route } from '../route'
import { useEventSubscription } from '../useEvents'
import { TrackList } from './TrackList'

type FolderRoute = Extract<Route, { tab: 'folders' }>

const ALBUM_SORT: Sort = { key: 'album', desc: false }

export function FolderTab({
  route,
  navigate,
  onOpenTrack,
}: {
  route: FolderRoute
  navigate: (r: Route, opts?: { replace?: boolean }) => void
  onOpenTrack: (row: TrackRow) => void
}) {
  const [albums, setAlbums] = useState<AlbumRow[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const load = useCallback(() => {
    listAlbums()
      .then((xs) => {
        setAlbums(xs)
        setError(null)
      })
      .catch((e: unknown) => setError(e instanceof ApiError ? `読み込みに失敗しました（${e.code}）` : '読み込みに失敗しました'))
  }, [])
  useEffect(() => load(), [load])
  const [refetch] = useState(() => new TrailingDebounce(() => load(), 500, 3000))
  useEffect(() => () => refetch.cancel(), [refetch])
  useEventSubscription({
    onLibrary: () => refetch.trigger(),
    onOpen: (reconnect) => {
      if (reconnect) refetch.trigger()
    },
  })

  const tree = useMemo(() => (albums ? buildTree(albums, FOLDER_PATTERN) : null), [albums])
  const level = useMemo(() => (tree ? folderLevel(tree, route.path) : null), [tree, route.path])
  // 消えた段は最上段へ戻す
  const gone = tree != null && level == null
  useEffect(() => {
    if (gone) navigate({ tab: 'folders', path: [] }, { replace: true })
  }, [gone, navigate])

  const albumId = level?.albumIds?.[0]
  const albumFilter = useMemo<Filter>(() => ({ album_id: albumId }), [albumId])

  if (error != null && albums == null) {
    return (
      <div className="m-center-row">
        <p className="m-error">{error}</p>
        <button type="button" onClick={load}>
          再読み込み
        </button>
      </div>
    )
  }
  if (albums == null || level == null) return <p className="m-placeholder">読み込み中…</p>

  const goto = (path: string[]) => navigate({ tab: 'folders', path })
  return (
    <div className="m-library">
      <nav className="m-crumbs" aria-label="パンくず">
        <button type="button" className="m-crumb" onClick={() => goto([])} disabled={route.path.length === 0}>
          Library
        </button>
        {route.path.map((label, i) => (
          <span key={i} className="m-crumb-item">
            <span aria-hidden>›</span>
            <button
              type="button"
              className="m-crumb"
              onClick={() => goto(route.path.slice(0, i + 1))}
              disabled={i === route.path.length - 1}
            >
              {label}
            </button>
          </span>
        ))}
      </nav>
      {level.albumIds != null ? (
        <TrackList filter={albumFilter} sort={ALBUM_SORT} onOpen={onOpenTrack} />
      ) : level.nodes.length === 0 ? (
        <p className="m-placeholder">フォルダはありません</p>
      ) : (
        <div className="m-scroll">
          <ul className="m-list">
            {level.nodes.map((n) => (
              <li key={n.key}>
                <button type="button" className="m-row" onClick={() => goto([...route.path, n.label])}>
                  <span className="m-row-main">
                    <span className="m-row-title">{n.label}</span>
                    <span className="m-row-sub">{formatCount(n.count)} 曲</span>
                  </span>
                  <span className="m-chevron" aria-hidden>
                    ›
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </div>
      )}
    </div>
  )
}
