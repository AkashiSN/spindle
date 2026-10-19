// ライブラリのタブ。上に検索欄、その下で アーティスト / アルバム / 曲 を切り替える。
// アーティスト → その albumartist のアルバム → アルバムの曲、と 1 段ずつ降りる（位置は route）。
// アーティストとアルバムは GET /api/albums からクライアント側で組む

import { useCallback, useEffect, useMemo, useState } from 'react'
import { ApiError } from '../../api/client'
import type { AlbumRow, TrackRow } from '../../api/types'
import { albumTitle } from '../../lib/artwork'
import { TrailingDebounce } from '../../lib/debounce'
import type { Filter, Sort } from '../../lib/filter'
import { formatCount } from '../../lib/format'
import { Latest } from '../../lib/latest'
import { listAlbums } from '../api'
import { albumsOf, artistName, artistsOf, loadLibraryView, saveLibraryView } from '../library'
import { hasExplicitLibraryView, type LibraryView, type Route } from '../route'
import { useEventSubscription } from '../useEvents'
import { AlbumArt, AlbumList } from './AlbumList'
import { ArtistList } from './ArtistList'
import { SearchBar } from './SearchBar'
import { Segmented } from './Segmented'
import { TrackList } from './TrackList'

type LibraryRoute = Extract<Route, { tab: 'library' }>

const VIEWS: readonly (readonly [LibraryView, string])[] = [
  ['artists', 'アーティスト'],
  ['albums', 'アルバム'],
  ['tracks', '曲'],
]

const ALBUM_SORT: Sort = { key: 'album', desc: false }
const TITLE_SORT: Sort = { key: 'title', desc: false }

export function LibraryTab({
  route,
  navigate,
  onOpenTrack,
}: {
  route: LibraryRoute
  navigate: (r: Route, opts?: { replace?: boolean }) => void
  onOpenTrack: (row: TrackRow) => void
}) {
  // URL に見方が無ければ（/m/ など）最後に選んだ見方にする
  useEffect(() => {
    if (!hasExplicitLibraryView(window.location.pathname)) {
      navigate({ ...route, view: loadLibraryView() }, { replace: true })
    }
  }, [route, navigate])

  const [albums, setAlbums] = useState<AlbumRow[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  // 取り直しが重なったとき、後から返った古い応答で上書きしない
  const [gen] = useState(() => new Latest())
  const load = useCallback(() => {
    const g = gen.next()
    listAlbums()
      .then((xs) => {
        if (!gen.isCurrent(g)) return
        setAlbums(xs)
        setError(null)
      })
      .catch((e: unknown) => {
        if (gen.isCurrent(g)) setError(e instanceof ApiError ? `読み込みに失敗しました（${e.code}）` : '読み込みに失敗しました')
      })
  }, [gen])
  useEffect(() => load(), [load])
  useEffect(() => () => gen.invalidate(), [gen])
  const [refetch] = useState(() => new TrailingDebounce(() => load(), 500, 3000))
  useEffect(() => () => refetch.cancel(), [refetch])
  useEventSubscription({
    onLibrary: () => refetch.trigger(),
    onOpen: (reconnect) => {
      if (reconnect) refetch.trigger()
    },
  })

  const q = route.q ?? ''
  const selectView = (view: LibraryView) => {
    saveLibraryView(view)
    navigate({ tab: 'library', view, ...(q !== '' ? { q } : {}) })
  }
  // 検索語の入力では履歴を積まない
  const setQuery = (text: string) => {
    const t = text.trim()
    const next: LibraryRoute = { ...route }
    if (t === '') delete next.q
    else next.q = t
    navigate(next, { replace: true })
  }

  const album = route.albumId != null ? (albums?.find((a) => a.id === route.albumId) ?? null) : null
  const albumFilter = useMemo<Filter>(() => ({ album_id: route.albumId }), [route.albumId])
  const trackFilter = useMemo<Filter>(() => ({ q }), [q])

  let body
  if (route.albumId != null) {
    body = (
      <>
        <div className="m-subhead">
          <AlbumArt hash={album?.artwork_hash ?? null} />
          <div className="m-row-main">
            <span className="m-row-title">{album ? albumTitle(album) : 'アルバム'}</span>
            {album && (
              <span className="m-row-sub">
                {artistName(album)} · {formatCount(album.track_count)} 曲
              </span>
            )}
          </div>
        </div>
        <TrackList filter={albumFilter} sort={ALBUM_SORT} onOpen={onOpenTrack} />
      </>
    )
  } else if (route.view === 'tracks') {
    body = <TrackList filter={trackFilter} sort={TITLE_SORT} onOpen={onOpenTrack} />
  } else if (error != null && albums == null) {
    body = (
      <div className="m-center-row">
        <p className="m-error">{error}</p>
        <button type="button" onClick={load}>
          再読み込み
        </button>
      </div>
    )
  } else if (albums == null) {
    body = <p className="m-placeholder">読み込み中…</p>
  } else if (route.view === 'artists' && route.albumartist == null) {
    body = (
      <div className="m-scroll">
        <ArtistList
          artists={artistsOf(albums, q)}
          onOpen={(name) => navigate({ tab: 'library', view: 'artists', albumartist: name })}
        />
      </div>
    )
  } else {
    const xs = albumsOf(albums, { albumartist: route.view === 'artists' ? route.albumartist : undefined, q })
    body = (
      <div className="m-scroll">
        {route.view === 'artists' && route.albumartist != null && <div className="m-count">{route.albumartist}</div>}
        <AlbumList albums={xs} onOpen={(a) => navigate({ tab: 'library', view: 'albums', albumId: a.id })} />
      </div>
    )
  }

  return (
    <div className="m-library">
      {route.albumId == null && <SearchBar value={q} onChange={setQuery} />}
      <Segmented options={VIEWS} value={route.view} onChange={selectView} />
      {body}
    </div>
  )
}
