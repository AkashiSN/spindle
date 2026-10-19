// プレイリストのタブ。一覧（普通 ♪ / スマート ✦）と、選んだプレイリストの中身（PlaylistDetail）。
// 位置は route（/m/playlists/{id}）。一覧は SSE の playlist で取り直す（曲数・長さは library でも変わるので間引いて取り直す）

import { useCallback, useEffect, useState } from 'react'
import { ApiError } from '../../api/client'
import type { Playlist, TrackRow } from '../../api/types'
import { TrailingDebounce } from '../../lib/debounce'
import { createPlaylist, listPlaylists } from '../api'
import { nameErrorMessage, playlistSubtitle } from '../playlists'
import type { Route } from '../route'
import { useEventSubscription } from '../useEvents'
import { NameSheet } from './NameSheet'
import { PlaylistDetail } from './PlaylistDetail'

type PlaylistsRoute = Extract<Route, { tab: 'playlists' }>

export function PlaylistsTab({
  route,
  navigate,
  onOpenTrack,
}: {
  route: PlaylistsRoute
  navigate: (r: Route, opts?: { replace?: boolean }) => void
  onOpenTrack: (row: TrackRow) => void
}) {
  const [lists, setLists] = useState<Playlist[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [creating, setCreating] = useState(false)

  const load = useCallback(() => {
    listPlaylists()
      .then((xs) => {
        setLists(xs)
        setError(null)
      })
      .catch((e: unknown) => setError(e instanceof ApiError ? `読み込みに失敗しました（${e.code}）` : '読み込みに失敗しました'))
  }, [])
  useEffect(() => load(), [load])
  const [refetch] = useState(() => new TrailingDebounce(() => load(), 500, 3000))
  useEffect(() => () => refetch.cancel(), [refetch])
  useEventSubscription({
    onPlaylist: () => load(),
    onLibrary: () => refetch.trigger(),
    onOpen: (reconnect) => {
      if (reconnect) load()
    },
  })

  const id = route.playlistId
  const current = id != null && lists ? (lists.find((p) => p.id === id) ?? null) : null
  // 消えた（他で削除された）プレイリストを開いていたら一覧へ戻す
  const gone = id != null && lists != null && current == null
  useEffect(() => {
    if (gone) navigate({ tab: 'playlists' }, { replace: true })
  }, [gone, navigate])

  if (id != null) {
    if (current == null)
      return <p className={error ? 'm-error m-sheet-pad' : 'm-placeholder'}>{error ?? '読み込み中…'}</p>
    return (
      <PlaylistDetail
        key={current.id}
        playlist={current}
        route={route}
        navigate={navigate}
        onOpenTrack={onOpenTrack}
        onChanged={load}
      />
    )
  }

  const create = async ({ name }: { name: string }): Promise<string | null> => {
    try {
      const p = await createPlaylist({ name })
      setCreating(false)
      setLists((xs) => (xs ? [...xs, p] : xs))
      load()
      navigate({ tab: 'playlists', playlistId: p.id })
      return null
    } catch (e) {
      return nameErrorMessage(e)
    }
  }

  return (
    <div className="m-library">
      <div className="m-subhead">
        <span className="m-row-main m-row-sub">{lists ? `${lists.length} 件` : ''}</span>
        <button type="button" className="m-icon" aria-label="プレイリストを作る" onClick={() => setCreating(true)}>
          ＋
        </button>
      </div>
      <div className="m-scroll">
        {error != null && (
          <div className="m-center-row">
            <p className="m-error">{error}</p>
            <button type="button" onClick={load}>
              再読み込み
            </button>
          </div>
        )}
        {lists == null && error == null && <p className="m-placeholder">読み込み中…</p>}
        {lists != null && lists.length === 0 && <p className="m-placeholder">プレイリストはまだありません</p>}
        {lists != null && lists.length > 0 && (
          <ul className="m-list">
            {lists.map((p) => (
              <li key={p.id}>
                <button
                  type="button"
                  className="m-row"
                  onClick={() => navigate({ tab: 'playlists', playlistId: p.id })}
                >
                  <span className="m-pl-icon" aria-label={p.kind === 'smart' ? 'スマート' : '普通'}>
                    {p.kind === 'smart' ? '✦' : '♪'}
                  </span>
                  <span className="m-row-main">
                    <span className="m-row-title">{p.name}</span>
                    <span className="m-row-sub">{playlistSubtitle(p)}</span>
                  </span>
                  <span className="m-chevron" aria-hidden>
                    ›
                  </span>
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>
      {creating && (
        <NameSheet
          label="プレイリストを作る"
          initial=""
          chooseKind
          submitLabel="作成"
          onSubmit={create}
          onClose={() => setCreating(false)}
        />
      )}
    </div>
  )
}
