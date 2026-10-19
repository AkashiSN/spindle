// 曲のシートの「プレイリストに追加」。普通のプレイリストを並べ、選ぶと末尾に足す（POST /api/playlists/:id/items）

import { useEffect, useState } from 'react'
import { ApiError } from '../../api/client'
import type { Playlist } from '../../api/types'
import { addToPlaylist, listPlaylists } from '../api'
import { playlistSubtitle } from '../playlists'
import { BottomSheet } from './BottomSheet'

type Result = { ok: boolean; text: string }

function failText(e: unknown, what: string): string {
  return e instanceof ApiError ? `${what}（${e.code}）` : what
}

export function AddToPlaylistSheet({ trackId, onClose }: { trackId: number; onClose: () => void }) {
  const [lists, setLists] = useState<Playlist[] | null>(null)
  const [loadError, setLoadError] = useState<string | null>(null)
  const [busy, setBusy] = useState<number | null>(null)
  const [result, setResult] = useState<Result | null>(null)

  useEffect(() => {
    let alive = true
    listPlaylists()
      .then((xs) => {
        if (alive) setLists(xs.filter((p) => p.kind === 'manual'))
      })
      .catch((e: unknown) => {
        if (alive) setLoadError(failText(e, '読み込みに失敗しました'))
      })
    return () => {
      alive = false
    }
  }, [])

  const add = (p: Playlist) => {
    if (busy != null) return
    setBusy(p.id)
    setResult(null)
    addToPlaylist(p.id, [trackId])
      .then((r) => {
        setResult(
          r.added > 0
            ? { ok: true, text: `「${p.name}」に追加しました` }
            : { ok: false, text: `「${p.name}」には既に入っています` },
        )
        if (r.added > 0)
          setLists((xs) =>
            xs ? xs.map((x) => (x.id === p.id ? { ...x, track_count: x.track_count + r.added } : x)) : xs,
          )
      })
      .catch((e: unknown) => setResult({ ok: false, text: failText(e, '追加できませんでした') }))
      .finally(() => setBusy(null))
  }

  return (
    <BottomSheet label="プレイリストに追加" expanded={false} onClose={onClose}>
      <h2 className="m-sheet-h">プレイリストに追加</h2>
      {result != null && <p className={result.ok ? 'm-notice m-notice-ok' : 'm-notice'}>{result.text}</p>}
      {loadError != null && <p className="m-error m-sheet-pad">{loadError}</p>}
      {lists == null && loadError == null && <p className="m-placeholder">読み込み中…</p>}
      {lists != null && lists.length === 0 && <p className="m-placeholder">普通のプレイリストがありません</p>}
      {lists != null && lists.length > 0 && (
        <ul className="m-list">
          {lists.map((p) => (
            <li key={p.id}>
              <button type="button" className="m-row" onClick={() => add(p)} disabled={busy != null}>
                <span className="m-pl-icon" aria-hidden>
                  ♪
                </span>
                <span className="m-row-main">
                  <span className="m-row-title">{p.name}</span>
                  <span className="m-row-sub">{busy === p.id ? '追加中…' : playlistSubtitle(p)}</span>
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
      <div className="m-sheet-actions">
        <button type="button" onClick={onClose}>
          閉じる
        </button>
      </div>
    </BottomSheet>
  )
}
