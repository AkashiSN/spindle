// プレイリストの中身。見出しに 戻る / 名前・曲数 / （普通なら）編集 / … メニュー（名前を変更・削除）。
// - 普段は TrackList（position 順）。SSE の playlist でこのプレイリストが変わったら取り直す
// - 普通のプレイリストの「編集」: 全件を読み、行に ⊖（外す）と ≡（長押しでドラッグして並べ替え）を出す
// - スマートは読むだけ。条件の編集は Task 9（route.edit = 'rule' | 'text'）

import { useCallback, useEffect, useRef, useState, type PointerEvent } from 'react'
import { ApiError } from '../../api/client'
import type { Playlist, TrackRow } from '../../api/types'
import type { Filter, Sort } from '../../lib/filter'
import { deletePlaylist, listTracks, moveInPlaylist, removeFromPlaylist, updatePlaylist } from '../api'
import { deleteErrorMessage, moveTarget, nameErrorMessage, playlistSubtitle } from '../playlists'
import type { Route } from '../route'
import { useEventSubscription } from '../useEvents'
import { ConfirmSheet } from './ConfirmSheet'
import { NameSheet } from './NameSheet'
import { TrackList } from './TrackList'

type PlaylistsRoute = Extract<Route, { tab: 'playlists' }>

const POSITION: Sort = { key: 'position', desc: false }

export function PlaylistDetail({
  playlist,
  route,
  navigate,
  onOpenTrack,
  onChanged,
}: {
  playlist: Playlist
  route: PlaylistsRoute
  navigate: (r: Route, opts?: { replace?: boolean }) => void
  onOpenTrack: (row: TrackRow) => void
  /** 名前・曲数が変わった（一覧を取り直す） */
  onChanged: () => void
}) {
  const [editing, setEditing] = useState(false)
  const [menuOpen, setMenuOpen] = useState(false)
  const [renaming, setRenaming] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [token, setToken] = useState(0)
  const filter: Filter = { playlist_id: playlist.id }
  const manual = playlist.kind === 'manual'

  useEventSubscription({
    onPlaylist: (e) => {
      if (e.playlist_ids.includes(playlist.id)) setToken((t) => t + 1)
    },
  })

  const finishEdit = () => {
    setEditing(false)
    setToken((t) => t + 1)
    onChanged()
  }

  const rename = async ({ name }: { name: string }): Promise<string | null> => {
    try {
      await updatePlaylist(playlist.id, { name })
      setRenaming(false)
      onChanged()
      return null
    } catch (e) {
      return nameErrorMessage(e)
    }
  }

  const remove = async (): Promise<string | null> => {
    try {
      await deletePlaylist(playlist.id)
      navigate({ tab: 'playlists' }, { replace: true })
      onChanged()
      return null
    } catch (e) {
      return deleteErrorMessage(e)
    }
  }

  return (
    <div className="m-library">
      <div className="m-subhead m-pl-head">
        <button type="button" className="m-icon" aria-label="一覧へ戻る" onClick={() => navigate({ tab: 'playlists' })}>
          ‹
        </button>
        <span className="m-row-main">
          <span className="m-row-title m-pl-name">
            <span aria-hidden>{manual ? '♪' : '✦'}</span> {playlist.name}
          </span>
          <span className="m-row-sub">{playlistSubtitle(playlist)}</span>
        </span>
        {manual &&
          (editing ? (
            <button type="button" className="m-primary" onClick={finishEdit}>
              完了
            </button>
          ) : (
            <button type="button" onClick={() => setEditing(true)} disabled={playlist.track_count === 0}>
              編集
            </button>
          ))}
        <span className="m-pl-menu-wrap">
          <button
            type="button"
            className="m-icon"
            aria-label="プレイリストの操作"
            aria-expanded={menuOpen}
            onClick={() => setMenuOpen((v) => !v)}
          >
            …
          </button>
          {menuOpen && (
            <>
              <div className="m-backdrop" onClick={() => setMenuOpen(false)} />
              <div className="m-menu" role="menu">
                <button
                  type="button"
                  role="menuitem"
                  onClick={() => {
                    setMenuOpen(false)
                    setRenaming(true)
                  }}
                >
                  名前を変更
                </button>
                <button
                  type="button"
                  role="menuitem"
                  className="m-menu-danger"
                  onClick={() => {
                    setMenuOpen(false)
                    setConfirmDelete(true)
                  }}
                >
                  削除
                </button>
              </div>
            </>
          )}
        </span>
      </div>

      {!manual && <SmartRuleSummary playlist={playlist} route={route} />}

      {editing ? (
        <EditableTracks playlistId={playlist.id} onChanged={onChanged} />
      ) : (
        <TrackList filter={filter} sort={POSITION} onOpen={onOpenTrack} refreshToken={token} />
      )}

      {renaming && (
        <NameSheet
          label="名前を変更"
          initial={playlist.name}
          chooseKind={false}
          submitLabel="保存"
          onSubmit={rename}
          onClose={() => setRenaming(false)}
        />
      )}
      {confirmDelete && (
        <ConfirmSheet
          label="プレイリストを削除"
          message={`『${playlist.name}』を削除します。曲のファイルは消えません`}
          confirmLabel="削除"
          danger
          onConfirm={remove}
          onClose={() => setConfirmDelete(false)}
        />
      )}
    </div>
  )
}

/**
 * スマートプレイリストのルール。Task 9 がここに「条件を編集」（navigate({ ...route, edit: 'rule' })）と
 * route.edit に応じた組み立て画面 / ルール文の画面を入れる
 */
function SmartRuleSummary({ playlist }: { playlist: Playlist; route: PlaylistsRoute }) {
  return (
    <div className="m-pl-rule">
      <code>{playlist.rule_source ?? ''}</code>
      <p className="m-note">条件の編集は PC で行ってください</p>
    </div>
  )
}

// ---------------------------------------------------------------- 編集（⊖ と ≡）

const ROW_H = 60
/** 長押しと判定するまでの時間（ms） */
const LONG_PRESS = 250
/** 長押しの前にこれ以上動いたら取りやめる（px） */
const SLOP = 8
/** 端からこの範囲に指があれば自動でスクロールする（px） */
const EDGE = 56
const SCROLL_STEP = 8

type Press = {
  pointerId: number
  index: number
  startY: number
  clientY: number
  scroll0: number
  timer: number | null
  active: boolean
}

type Drag = { from: number; to: number; dy: number }

function trackTitle(r: TrackRow): string {
  return r.title || (r.rel_path.split('/').pop() ?? r.rel_path)
}

/** プレイリストの全件（position 順）。編集のときだけ読む */
async function loadAll(playlistId: number): Promise<TrackRow[]> {
  const out: TrackRow[] = []
  let cursor: string | null = null
  for (;;) {
    const page = await listTracks({ playlist_id: playlistId }, POSITION, cursor)
    out.push(...page.items)
    cursor = page.next_cursor
    if (cursor == null) return out
  }
}

function EditableTracks({ playlistId, onChanged }: { playlistId: number; onChanged: () => void }) {
  const [rows, setRows] = useState<TrackRow[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [drag, setDrag] = useState<Drag | null>(null)
  const gen = useRef(0)
  const scrollRef = useRef<HTMLDivElement>(null)
  const press = useRef<Press | null>(null)
  const raf = useRef<number | null>(null)
  const rowsRef = useRef<TrackRow[] | null>(null)
  useEffect(() => {
    rowsRef.current = rows
  })

  const reload = useCallback(() => {
    const g = ++gen.current
    loadAll(playlistId)
      .then((xs) => {
        if (g === gen.current) setRows(xs)
      })
      .catch((e: unknown) => {
        if (g === gen.current) setError(e instanceof ApiError ? `読み込みに失敗しました（${e.code}）` : '読み込みに失敗しました')
      })
  }, [playlistId])
  useEffect(() => reload(), [reload])
  useEventSubscription({
    onPlaylist: (e) => {
      if (e.playlist_ids.includes(playlistId) && press.current == null) reload()
    },
  })

  const failed = (what: string) => (e: unknown) => {
    setError(e instanceof ApiError ? `${what}（${e.code}）` : what)
    reload()
  }

  const removeRow = (row: TrackRow) => {
    setError(null)
    setRows((xs) => (xs ? xs.filter((x) => x.id !== row.id) : xs))
    removeFromPlaylist(playlistId, [row.id])
      .then(onChanged)
      .catch(failed('外せませんでした'))
  }

  // ---- ドラッグ
  /** 指の位置（とスクロール量）から、動かしている行の行き先 */
  const dragOf = (p: Press): Drag | null => {
    const el = scrollRef.current
    const n = rowsRef.current?.length ?? 0
    if (!p.active || !el || n === 0) return null
    const dy = p.clientY - p.startY + (el.scrollTop - p.scroll0)
    const to = Math.max(0, Math.min(n - 1, p.index + Math.round(dy / ROW_H)))
    return { from: p.index, to, dy }
  }
  const update = () => {
    const d = press.current ? dragOf(press.current) : null
    if (d) setDrag(d)
  }
  const stopAutoScroll = () => {
    if (raf.current != null) cancelAnimationFrame(raf.current)
    raf.current = null
  }
  const autoScroll = () => {
    const p = press.current
    const el = scrollRef.current
    if (!p || !p.active || !el) {
      raf.current = null
      return
    }
    const rect = el.getBoundingClientRect()
    if (p.clientY < rect.top + EDGE) el.scrollTop -= SCROLL_STEP
    else if (p.clientY > rect.bottom - EDGE) el.scrollTop += SCROLL_STEP
    update()
    raf.current = requestAnimationFrame(autoScroll)
  }
  useEffect(
    () => () => {
      stopAutoScroll()
      if (press.current?.timer != null) clearTimeout(press.current.timer)
    },
    [],
  )

  const endPress = () => {
    const p = press.current
    if (p?.timer != null) clearTimeout(p.timer)
    press.current = null
    stopAutoScroll()
    setDrag(null)
  }

  const onPointerDown = (e: PointerEvent<HTMLElement>, index: number) => {
    if (press.current) return
    e.currentTarget.setPointerCapture(e.pointerId)
    const p: Press = {
      pointerId: e.pointerId,
      index,
      startY: e.clientY,
      clientY: e.clientY,
      scroll0: scrollRef.current?.scrollTop ?? 0,
      timer: null,
      active: false,
    }
    p.timer = window.setTimeout(() => {
      p.timer = null
      p.active = true
      // 長押しの間にスクロールしていれば基準を取り直す
      p.scroll0 = scrollRef.current?.scrollTop ?? p.scroll0
      setDrag({ from: index, to: index, dy: p.clientY - p.startY })
      navigator.vibrate?.(10)
      raf.current = requestAnimationFrame(autoScroll)
    }, LONG_PRESS)
    press.current = p
  }
  const onPointerMove = (e: PointerEvent<HTMLElement>) => {
    const p = press.current
    if (!p || p.pointerId !== e.pointerId) return
    p.clientY = e.clientY
    if (!p.active) {
      if (Math.abs(e.clientY - p.startY) > SLOP) endPress()
      return
    }
    update()
  }
  const onPointerUp = (e: PointerEvent<HTMLElement>) => {
    const p = press.current
    if (!p || p.pointerId !== e.pointerId) return
    p.clientY = e.clientY
    const d = dragOf(p)
    endPress()
    const xs = rowsRef.current
    if (!p.active || !d || !xs || d.to === d.from) return
    const { trackId, before } = moveTarget(
      xs.map((r) => r.id),
      d.from,
      d.to,
    )
    const next = [...xs]
    const [moved] = next.splice(d.from, 1)
    if (moved) next.splice(d.to, 0, moved)
    setRows(next)
    setError(null)
    moveInPlaylist(playlistId, [trackId], before).catch(failed('並べ替えられませんでした'))
  }

  if (rows == null)
    return <p className={error ? 'm-error m-sheet-pad' : 'm-placeholder'}>{error ?? '読み込み中…'}</p>

  const shift = (i: number): number => {
    if (!drag || i === drag.from) return 0
    if (drag.from < drag.to && i > drag.from && i <= drag.to) return -ROW_H
    if (drag.to < drag.from && i >= drag.to && i < drag.from) return ROW_H
    return 0
  }

  return (
    <div className="m-tracklist">
      <div className="m-count">≡ を長押ししてドラッグで並べ替え、⊖ で外します（曲のファイルは消えません）</div>
      {error != null && <p className="m-error m-sheet-pad">{error}</p>}
      <div className="m-scroll" ref={scrollRef}>
        {rows.length === 0 && <p className="m-placeholder">曲はありません</p>}
        <ul className="m-list m-edit-list">
          {rows.map((r, i) => {
            const dragging = drag?.from === i
            const sub = [r.artist_display, r.album].filter((v): v is string => !!v).join(' · ')
            return (
              <li
                key={r.id}
                className={dragging ? 'm-row m-edit-row dragging' : 'm-row m-edit-row'}
                style={{
                  height: ROW_H,
                  transform: `translateY(${dragging && drag ? drag.dy : shift(i)}px)`,
                }}
              >
                <button
                  type="button"
                  className="m-remove"
                  aria-label={`「${trackTitle(r)}」を外す`}
                  onClick={() => removeRow(r)}
                  disabled={drag != null}
                >
                  ⊖
                </button>
                <span className="m-row-main">
                  <span className="m-row-title">{trackTitle(r)}</span>
                  <span className="m-row-sub">{sub}</span>
                </span>
                <span
                  className="m-handle"
                  role="button"
                  aria-label="長押しでドラッグして並べ替え"
                  onPointerDown={(e) => onPointerDown(e, i)}
                  onPointerMove={onPointerMove}
                  onPointerUp={onPointerUp}
                  onPointerCancel={endPress}
                  onContextMenu={(e) => e.preventDefault()}
                >
                  ≡
                </span>
              </li>
            )
          })}
        </ul>
      </div>
    </div>
  )
}
