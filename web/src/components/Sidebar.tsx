// 左サイドバー（SPEC §12.1、D-58）: ツリー（表示形式はパターン。組み込み 4 + ユーザ定義）/
// プレイリスト / 固定フィルタ。どれを選んでも中心の表のフィルタ（scope）を差し替えるだけ。
// ツリーのノードは配下の album id の集合（filter.album_ids）で絞る

import { useMemo, useState } from 'react'
import type { AlbumRow, Playlist } from '../api/types'
import type { Playlists } from '../hooks/usePlaylists'
import { useLocalStorageState } from '../hooks/useLocalStorageState'
import { FLAGS, FLAG_LABELS, filterToParam, type Filter, type Flag } from '../lib/filter'
import { formatCount } from '../lib/format'
import { buildTree, parsePattern, PRESETS, TREE_FIELDS, type TreeNode, type TreePreset } from '../lib/tree'
import { PlaylistSection } from './PlaylistSection'

/** サイドバーが決める部分。検索語 q は上部ナビが別に持つ */
export type Scope = Omit<Filter, 'q'>

const isPresetList = (v: unknown): v is TreePreset[] =>
  Array.isArray(v) &&
  v.every((x) => typeof x === 'object' && x != null && typeof x.name === 'string' && typeof x.pattern === 'string')

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
  // 表示形式: 組み込みのプリセット + ユーザ定義（localStorage）。選択中はパターン文字列で覚える
  const [custom, setCustom] = useLocalStorageState<TreePreset[]>('tree.custom', [], isPresetList)
  const [mode, setMode] = useLocalStorageState<string>(
    'tree.mode',
    PRESETS[0].pattern,
    (v): v is string => typeof v === 'string',
  )
  const views = useMemo(() => [...PRESETS, ...custom], [custom])
  const active = views.find((v) => v.pattern === mode) ?? PRESETS[0]
  const parsed = useMemo(() => parsePattern(active.pattern), [active.pattern])
  const tree = useMemo(() => ('error' in parsed ? [] : buildTree(albums, parsed)), [albums, parsed])
  const total = useMemo(
    () => albums.reduce((n, a) => (a.missing_since == null ? n + a.track_count : n), 0),
    [albums],
  )
  const [editing, setEditing] = useState<{ original: TreePreset | null; name: string; pattern: string } | null>(
    null,
  )
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

  const saveView = () => {
    if (!editing) return
    const name = editing.name.trim()
    const p = parsePattern(editing.pattern)
    if (!name || 'error' in p) return
    const view = { name, pattern: editing.pattern.trim() }
    setCustom((prev) => {
      const rest = editing.original ? prev.filter((v) => v !== editing.original && v.name !== editing.original?.name) : prev
      return [...rest.filter((v) => v.name !== name), view]
    })
    setMode(view.pattern)
    setEditing(null)
  }
  const removeView = () => {
    if (PRESETS.some((p) => p.pattern === active.pattern)) return
    setCustom((prev) => prev.filter((v) => v.pattern !== active.pattern))
    setMode(PRESETS[0].pattern)
  }
  const editingError = editing ? (() => {
    const p = parsePattern(editing.pattern)
    return 'error' in p ? p.error : editing.name.trim() === '' ? '名前が空' : null
  })() : null

  const renderNodes = (nodes: TreeNode[], depth: number) => (
    <ul className={depth === 0 ? 'tree' : ''}>
      {nodes.map((n) => {
        const nodeScope: Scope = { album_ids: n.albumIds }
        const leaf = n.children.length === 0
        return (
          <li key={n.key}>
            <div className="tree-row">
              {leaf ? (
                <span className="twisty" />
              ) : (
                <button type="button" className="twisty" onClick={() => toggle(n.key)}>
                  {open.has(n.key) ? '▾' : '▸'}
                </button>
              )}
              <button
                type="button"
                className={`tree-node${leaf ? ' leaf' : ''}${is(nodeScope) ? ' active' : ''}`}
                title={n.label}
                onClick={() => onScope(nodeScope)}
              >
                {n.label}
                <span className="muted"> ({formatCount(n.count)})</span>
              </button>
            </div>
            {!leaf && open.has(n.key) && renderNodes(n.children, depth + 1)}
          </li>
        )
      })}
    </ul>
  )

  return (
    <nav className="sidebar">
      <section className="tree-section">
        <h2>
          <button
            type="button"
            className={`tree-node root${is({}) ? ' active' : ''}`}
            onClick={() => onScope({})}
          >
            All Music <span className="muted">({formatCount(total)})</span>
          </button>
        </h2>
        {'error' in parsed ? <p className="error small">{parsed.error}</p> : renderNodes(tree, 0)}
        <div className="tree-mode">
          <select
            value={active.pattern}
            title={active.pattern}
            onChange={(e) => {
              if (e.target.value === '__new__') {
                setEditing({ original: null, name: '', pattern: '%albumartist%|%album%' })
                return
              }
              setMode(e.target.value)
            }}
          >
            {views.map((v) => (
              <option key={v.name} value={v.pattern}>
                {v.name}
              </option>
            ))}
            <option value="__new__">新しい表示形式…</option>
          </select>
          {!PRESETS.some((p) => p.pattern === active.pattern) && (
            <>
              <button
                type="button"
                className="ghost"
                title="この表示形式を編集"
                onClick={() => setEditing({ original: active, name: active.name, pattern: active.pattern })}
              >
                ✎
              </button>
              <button type="button" className="ghost" title="この表示形式を削除" onClick={removeView}>
                ×
              </button>
            </>
          )}
        </div>
        {editing && (
          <div className="tree-editor">
            <input
              placeholder="名前"
              value={editing.name}
              onChange={(e) => setEditing({ ...editing, name: e.target.value })}
            />
            <input
              placeholder="%category%|%albumartist%|%album%"
              value={editing.pattern}
              onChange={(e) => setEditing({ ...editing, pattern: e.target.value })}
              onKeyDown={(e) => {
                if (e.key === 'Enter') saveView()
                if (e.key === 'Escape') setEditing(null)
              }}
            />
            <div className="muted small">
              | で階層、%field% で差し込み、[ ] は空なら省略。フィールド: {TREE_FIELDS.join(' ')}
            </div>
            {editingError && <div className="error small">{editingError}</div>}
            <div>
              <button type="button" disabled={editingError != null} onClick={saveView}>
                保存
              </button>{' '}
              <button type="button" className="ghost" onClick={() => setEditing(null)}>
                取消
              </button>
            </div>
          </div>
        )}
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
