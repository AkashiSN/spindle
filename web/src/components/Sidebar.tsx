// 左サイドバー（SPEC §12.1）: ツリー（Category → AlbumArtist → Album）/ プレイリスト / 固定フィルタ。
// どれを選んでも中心の表のフィルタ（scope）を差し替えるだけ

import { useMemo, useState } from 'react'
import type { AlbumRow, Playlist } from '../api/types'
import type { Playlists } from '../hooks/usePlaylists'
import { FLAGS, FLAG_LABELS, filterToParam, type Filter, type Flag } from '../lib/filter'
import { PlaylistSection } from './PlaylistSection'

/** サイドバーが決める部分。検索語 q は上部ナビが別に持つ */
export type Scope = Omit<Filter, 'q'>

type Tree = Map<string, Map<string, AlbumRow[]>>

const NO_CATEGORY = '（カテゴリなし）'
const NO_ARTIST = '（アルバムアーティストなし）'

function buildTree(albums: AlbumRow[]): Tree {
  const tree: Tree = new Map()
  for (const a of albums) {
    if (a.missing_since != null) continue
    const c = a.category ?? NO_CATEGORY
    const aa = a.albumartist ?? NO_ARTIST
    let byArtist = tree.get(c)
    if (!byArtist) tree.set(c, (byArtist = new Map()))
    let list = byArtist.get(aa)
    if (!list) byArtist.set(aa, (list = []))
    list.push(a)
  }
  return tree
}

const collator = new Intl.Collator('ja')

export function Sidebar({
  albums,
  scope,
  onScope,
  playlists,
  onPlaylistDeleted,
  onDropTracks,
  onEditRule,
  onRefreshed,
  playlistNotice,
  onPlaylistNotice,
}: {
  albums: AlbumRow[]
  scope: Scope
  onScope: (s: Scope) => void
  playlists: Playlists
  onPlaylistDeleted: (playlistId: number) => void
  onDropTracks: (playlistId: number, trackIds: number[]) => void
  onEditRule: (p: Playlist | null) => void
  onRefreshed: (playlistId: number) => void
  playlistNotice: string | null
  onPlaylistNotice: (text: string | null) => void
}) {
  const tree = useMemo(() => buildTree(albums), [albums])
  const [open, setOpen] = useState<Set<string>>(() => new Set())
  const toggle = (key: string) =>
    setOpen((prev) => {
      const next = new Set(prev)
      if (next.has(key)) next.delete(key)
      else next.add(key)
      return next
    })
  const current = filterToParam(scope)
  const is = (s: Scope) => filterToParam(s) === current
  const activeFlags = new Set(scope.flags ?? [])

  return (
    <nav className="sidebar">
      <section>
        <h2>
          <button
            type="button"
            className={`tree-node root${is({}) ? ' active' : ''}`}
            onClick={() => onScope({})}
          >
            すべて
          </button>
        </h2>
        <ul className="tree">
          {[...tree.keys()].sort(collator.compare).map((cat) => {
            const byArtist = tree.get(cat)!
            const catScope: Scope = cat === NO_CATEGORY ? {} : { category: cat }
            const catKey = `c:${cat}`
            return (
              <li key={cat}>
                <div className="tree-row">
                  <button type="button" className="twisty" onClick={() => toggle(catKey)}>
                    {open.has(catKey) ? '▾' : '▸'}
                  </button>
                  <button
                    type="button"
                    className={`tree-node${cat !== NO_CATEGORY && is(catScope) ? ' active' : ''}`}
                    disabled={cat === NO_CATEGORY}
                    onClick={() => onScope(catScope)}
                  >
                    {cat}
                  </button>
                </div>
                {open.has(catKey) && (
                  <ul>
                    {[...byArtist.keys()].sort(collator.compare).map((aa) => {
                      const list = byArtist.get(aa)!
                      const aaKey = `${catKey}/a:${aa}`
                      const aaScope: Scope =
                        aa === NO_ARTIST ? catScope : { ...catScope, albumartist: aa }
                      return (
                        <li key={aa}>
                          <div className="tree-row">
                            <button type="button" className="twisty" onClick={() => toggle(aaKey)}>
                              {open.has(aaKey) ? '▾' : '▸'}
                            </button>
                            <button
                              type="button"
                              className={`tree-node${aa !== NO_ARTIST && is(aaScope) ? ' active' : ''}`}
                              disabled={aa === NO_ARTIST}
                              onClick={() => onScope(aaScope)}
                            >
                              {aa}
                            </button>
                          </div>
                          {open.has(aaKey) && (
                            <ul>
                              {list
                                .slice()
                                .sort((x, y) => collator.compare(x.album ?? '', y.album ?? ''))
                                .map((al) => (
                                  <li key={al.id}>
                                    <button
                                      type="button"
                                      className={`tree-node leaf${is({ album_id: al.id }) ? ' active' : ''}`}
                                      title={al.rel_dir}
                                      onClick={() => onScope({ album_id: al.id })}
                                    >
                                      {al.album ?? al.rel_dir}
                                      <span className="muted"> {al.track_count}</span>
                                    </button>
                                  </li>
                                ))}
                            </ul>
                          )}
                        </li>
                      )
                    })}
                  </ul>
                )}
              </li>
            )
          })}
        </ul>
      </section>

      <PlaylistSection
        playlists={playlists}
        activeId={scope.playlist_id ?? null}
        onOpen={(id) => onScope({ playlist_id: id })}
        onDeleted={onPlaylistDeleted}
        onDropTracks={onDropTracks}
        onEditRule={onEditRule}
        onRefreshed={onRefreshed}
        notice={playlistNotice}
        onNotice={onPlaylistNotice}
      />

      <section>
        <h2>フィルタ</h2>
        <ul className="flat">
          {FLAGS.map((flag: Flag) => (
            <li key={flag}>
              <button
                type="button"
                className={`tree-node${activeFlags.has(flag) ? ' active' : ''}`}
                onClick={() => {
                  // 固定フィルタはトグル。ツリーの絞り込みと組み合わせられる（AND）
                  const flags = new Set(scope.flags ?? [])
                  if (flags.has(flag)) flags.delete(flag)
                  else flags.add(flag)
                  onScope({ ...scope, flags: [...flags] })
                }}
              >
                {FLAG_LABELS[flag]}
              </button>
            </li>
          ))}
        </ul>
      </section>
    </nav>
  )
}
