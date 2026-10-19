// プレイリストの中身。見出しに 戻る / 名前・曲数 / （普通なら）編集 / … メニュー（名前を変更・削除）。
// - 普段は TrackList（position 順）。SSE の playlist でこのプレイリストが変わったら取り直す
// - 普通のプレイリストの「編集」: 全件を読み、行に ⊖（外す）と ≡（長押しでドラッグして並べ替え）を出す
// - スマートはルールの原文と「条件を編集」。route.edit = 'rule'（組み立て画面）| 'text'（ルール文の画面）で
//   編集画面に切り替える。組み立て画面で表せないルールは開いた時点でルール文の画面に移す（置き換え）

import { useCallback, useEffect, useRef, useState, type PointerEvent } from 'react'
import { ApiError } from '../../api/client'
import type { Playlist, TrackRow } from '../../api/types'
import { DEFAULT_SORT, type Filter, type Sort } from '../../lib/filter'
import {
  deletePlaylist,
  listTracks,
  moveInPlaylist,
  previewRule,
  removeFromPlaylist,
  updatePlaylist,
} from '../api'
import { deleteErrorMessage, moveTarget, nameErrorMessage, playlistSubtitle } from '../playlists'
import type { Route } from '../route'
import { astToModel, type BuilderModel } from '../ruleBuilder'
import { useRulePreview } from '../useRulePreview'
import { useEventSubscription } from '../useEvents'
import { ConfirmSheet } from './ConfirmSheet'
import { NameSheet } from './NameSheet'
import { RuleTextEditor } from './RuleTextEditor'
import { SmartEditor } from './SmartEditor'
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
  // 編集画面をこの画面から積んだか（積んだなら閉じるときは戻る。URL から直に開いたなら置き換える）
  const pushedEdit = useRef(false)
  useEffect(() => {
    if (route.edit == null) pushedEdit.current = false
  }, [route.edit])

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
      onChanged()
      return null
    } catch (e) {
      return nameErrorMessage(e)
    }
  }

  const remove = async (): Promise<string | null | (() => void)> => {
    try {
      await deletePlaylist(playlist.id)
      // シートの段を戻してから一覧へ
      return () => {
        navigate({ tab: 'playlists' }, { replace: true })
        onChanged()
      }
    } catch (e) {
      return deleteErrorMessage(e)
    }
  }

  if (!manual && route.edit != null) {
    const leave = () => {
      if (pushedEdit.current) {
        pushedEdit.current = false
        window.history.back()
      } else navigate({ tab: 'playlists', playlistId: playlist.id }, { replace: true })
    }
    return (
      <SmartEditScreen
        playlist={playlist}
        mode={route.edit}
        onMode={(edit) => navigate({ ...route, edit }, { replace: true })}
        onLeave={leave}
        onChanged={onChanged}
        onOpenTrack={onOpenTrack}
      />
    )
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

      {!manual && (
        <SmartRuleSummary
          playlist={playlist}
          onEdit={() => {
            pushedEdit.current = true
            navigate({ ...route, edit: 'rule' })
          }}
        />
      )}

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

/** スマートプレイリストのルールの原文と「条件を編集」 */
function SmartRuleSummary({ playlist, onEdit }: { playlist: Playlist; onEdit: () => void }) {
  return (
    <div className="m-pl-rule">
      <code>{playlist.rule_source ?? ''}</code>
      <button type="button" className="m-pl-rule-edit" onClick={onEdit}>
        条件を編集 ›
      </button>
    </div>
  )
}

// ---------------------------------------------------------------- 条件の編集（組み立て画面 / ルール文の画面）

type BuilderDraft = { dsl: string } | { error: string }

/**
 * 編集中の文（draft）は 2 つの画面で共有する。組み立て画面は DSL にできたときだけ draft を更新し、
 * できなければ文言（draftError）を出して保存させない。組み立て画面へは、今の文が組み立てで表せるときだけ戻れる
 */
function SmartEditScreen({
  playlist,
  mode,
  onMode,
  onLeave,
  onChanged,
  onOpenTrack,
}: {
  playlist: Playlist
  mode: 'rule' | 'text'
  onMode: (m: 'rule' | 'text') => void
  onLeave: () => void
  onChanged: () => void
  onOpenTrack: (row: TrackRow) => void
}) {
  const [draft, setDraft] = useState(playlist.rule_source ?? '')
  const [draftError, setDraftError] = useState<string | null>(null)
  // 組み立て画面の初期値（undefined は読み込み中）。画面を切り替えるたびに作り直すので key も変える
  const [model, setModel] = useState<BuilderModel | undefined>(undefined)
  const [modelKey, setModelKey] = useState(0)
  const [saving, setSaving] = useState(false)
  const [saveError, setSaveError] = useState<string | null>(null)
  const preview = useRulePreview(draftError != null ? null : draft)
  const rule = draft.trim()
  // 下の一覧は最後に通った文で出す（確かめ中に一覧を消さない）
  const [listRule, setListRule] = useState<string | null>(null)
  if (preview.kind === 'ok' && listRule !== rule) setListRule(rule)

  // 組み立て画面で開いたのに初期値が無い（URL から直に開いた・一覧から開いた）。AST を取って組み立てる。
  // 表せなければルール文の画面へ置き換える
  const needModel = mode === 'rule' && model === undefined
  useEffect(() => {
    if (!needModel) return
    const ac = new AbortController()
    previewRule(rule, ac.signal)
      .then((r) => {
        if (ac.signal.aborted) return
        const m = astToModel(r.ast)
        if (m) setModel(m)
        else onMode('text')
      })
      .catch(() => {
        if (!ac.signal.aborted) onMode('text')
      })
    return () => ac.abort()
    // 開いたときの 1 回だけ（rule を追わない）
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [needModel])

  const okModel = preview.kind === 'ok' ? astToModel(preview.ast) : null
  const toBuilder = () => {
    if (!okModel) return
    setModel(okModel)
    setModelKey((k) => k + 1)
    onMode('rule')
  }
  const toText = () => {
    // DSL にできない入力（値に " など）は捨て、最後に DSL にできた文で開く
    setDraftError(null)
    setModel(undefined)
    onMode('text')
  }
  const onBuilderDraft = (r: BuilderDraft) => {
    if ('dsl' in r) {
      setDraft(r.dsl)
      setDraftError(null)
    } else setDraftError(r.error)
  }

  const canSave = !saving && draftError == null && rule !== '' && preview.kind === 'ok'
  const save = () => {
    if (!canSave) return
    setSaving(true)
    setSaveError(null)
    updatePlaylist(playlist.id, { rule })
      .then(() => {
        onChanged()
        onLeave()
      })
      .catch((e: unknown) => {
        setSaveError(
          e instanceof ApiError ? (e.status === 400 ? e.message : `保存できませんでした（${e.code}）`) : '保存できませんでした',
        )
        setSaving(false)
      })
  }

  return (
    <div className="m-library">
      <div className="m-subhead m-pl-head">
        <button type="button" className="m-icon" aria-label="編集をやめて戻る" onClick={onLeave}>
          ‹
        </button>
        <span className="m-row-main">
          <span className="m-row-title m-pl-name">
            <span aria-hidden>✦</span> {playlist.name}
          </span>
          <span className="m-row-sub">{mode === 'rule' ? '条件を編集' : 'ルール文を編集'}</span>
        </span>
        <button type="button" className="m-primary" onClick={save} disabled={!canSave}>
          {saving ? '保存中…' : '保存'}
        </button>
      </div>
      <div className="m-rule-edit">
        {mode === 'rule' ? (
          model === undefined ? (
            <p className="m-placeholder">読み込み中…</p>
          ) : (
            <SmartEditor key={modelKey} initial={model} preview={preview} onDraft={onBuilderDraft} />
          )
        ) : (
          <RuleTextEditor value={draft} preview={preview} onChange={setDraft} />
        )}
        {saveError != null && <p className="m-error m-rule-status">{saveError}</p>}
        <div className="m-rule-switch">
          {mode === 'rule' ? (
            <button type="button" onClick={toText}>
              文で編集する ›
            </button>
          ) : (
            <>
              <button type="button" onClick={toBuilder} disabled={!okModel}>
                組み立て画面で編集する ›
              </button>
              {preview.kind === 'ok' && !okModel && (
                <p className="m-note">括弧の入れ子や AND と OR の混在は組み立て画面では表せません</p>
              )}
            </>
          )}
        </div>
      </div>
      {listRule != null && (
        <>
          <div className="m-count">一致する曲（並び順・上限は保存後に効きます）</div>
          <TrackList filter={{ dsl: listRule }} sort={DEFAULT_SORT} onOpen={onOpenTrack} />
        </>
      )}
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
