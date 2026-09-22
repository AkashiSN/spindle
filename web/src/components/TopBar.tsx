// 上部バー（SPEC §12.1、D-58 追記、P4-20）。ナビとプレイヤーを 1 本にまとめる:
// 左に画面切替（「ライブラリ」がホーム。取り込みタブの右の「Inbox」に承認待ちの赤い件数）、
// 中央に ◀◀ ▶ ▶▶ ■ とシーク、その右に曲名（無ければ Not playing）、右に RG / 原本 / 音量と
// ☰（ジョブ / 履歴 / 設定 / ログアウト。SSE の接続状態もここ）。
//
// ジョブが ☰ に入ったので、実行中 + 待ちの件数はメニュー内の「ジョブ」に添え、失敗の赤点は ☰ の
// ボタン自体に出す（SSE の点と合わせて 2 つまで。3 つ並べると読めない）

import { useEffect, useRef, useState } from 'react'
import type { JobSummary } from '../api/types'
import type { InboxSummary } from '../hooks/useInboxSummary'
import type { PlayerHandle } from '../hooks/usePlayer'
import { formatCount } from '../lib/format'
import { formatTime, isRgMode } from '../lib/playback'
import { MENU_VIEWS, VIEWS, type View } from '../lib/views'

export function TopBar({
  view,
  onView,
  summary,
  inbox,
  connected,
  onLogout,
  player,
}: {
  view: View
  onView: (v: View) => void
  summary: JobSummary | null
  /** Inbox の承認待ち / 失敗の件数（P4-20）。取れていなければバッジを出さない */
  inbox: InboxSummary | null
  connected: boolean
  onLogout: () => void
  player: PlayerHandle
}) {
  const { track, playing, position, duration } = player
  const max = duration ?? (track?.duration_ms != null ? track.duration_ms / 1000 : 0)
  const [menuOpen, setMenuOpen] = useState(false)
  const menuRef = useRef<HTMLDivElement>(null)
  // メニューは外側のクリックと Esc で閉じる
  useEffect(() => {
    if (!menuOpen) return
    const onDown = (e: MouseEvent) => {
      if (menuRef.current && !menuRef.current.contains(e.target as Node)) setMenuOpen(false)
    }
    const onKey = (e: KeyboardEvent) => {
      if (e.key === 'Escape') setMenuOpen(false)
    }
    document.addEventListener('mousedown', onDown)
    document.addEventListener('keydown', onKey)
    return () => {
      document.removeEventListener('mousedown', onDown)
      document.removeEventListener('keydown', onKey)
    }
  }, [menuOpen])

  const active = summary ? summary.running + summary.queued : 0
  const jobsTitle = summary
    ? `実行中 ${formatCount(summary.running)} · 待ち ${formatCount(summary.queued)} · 反映待ち ${formatCount(summary.pending_ops)} · 失敗 ${formatCount(summary.failed)}`
    : 'ジョブ要約を取得中…'
  const inboxTitle = inbox
    ? `承認待ち ${formatCount(inbox.pending)} 件 · 失敗 ${formatCount(inbox.failed)} 件`
    : 'Inbox（取り込んだものはここに集まる）'

  return (
    <header className="top-bar">
      <span className="brand">spindle</span>
      <nav className="views">
        {VIEWS.map(([v, label]) => (
          <button
            key={v}
            type="button"
            className={v === view ? 'active' : ''}
            onClick={() => onView(v)}
            title={v === 'inbox' ? inboxTitle : undefined}
          >
            {label}
            {v === 'inbox' && inbox ? (
              <>
                {inbox.pending > 0 ? <span className="inbox-count">{formatCount(inbox.pending)}</span> : null}
                {inbox.failed > 0 ? <span className="jobs-failed" aria-label="Inbox に失敗あり" /> : null}
              </>
            ) : null}
          </button>
        ))}
      </nav>
      <div className="transport">
        <button type="button" className="ghost" disabled={!track} onClick={player.prev} title="前の曲">
          ⏮
        </button>
        <button
          type="button"
          className="ghost play-toggle"
          disabled={!track}
          onClick={player.toggle}
          title={playing ? '一時停止' : '再生'}
        >
          {playing ? '⏸' : '▶'}
        </button>
        <button type="button" className="ghost" disabled={!track} onClick={player.next} title="次の曲">
          ⏭
        </button>
        <button type="button" className="ghost" disabled={!track} onClick={player.stop} title="停止">
          ■
        </button>
      </div>
      <div className="seek-row">
        <span className="time muted small">{formatTime(position)}</span>
        <input
          type="range"
          className="seek"
          min={0}
          max={max > 0 ? max : 1}
          step={0.5}
          value={Math.min(position, max > 0 ? max : 1)}
          disabled={!track}
          onChange={(e) => player.seek(Number(e.target.value))}
          title={player.transcoding ? '変換中（シークは読み直し）' : 'シーク'}
        />
        <span className="time muted small">{formatTime(max)}</span>
      </div>
      <div className="now-playing" title={track?.rel_path}>
        {player.error ? (
          <span className="error">{player.error}</span>
        ) : track ? (
          <>
            <span className="np-title">{track.title ?? track.rel_path}</span>
            {track.artist_display ? <span className="muted"> — {track.artist_display}</span> : null}
            {player.transcoding ? <span className="muted small"> （変換中）</span> : null}
          </>
        ) : (
          <span className="muted">Not playing</span>
        )}
      </div>
      <div className="player-options">
        <label className="small" title="ReplayGain の掛け方。album は album gain が無ければ track gain">
          RG
          <select
            className="rg-mode"
            value={player.rgMode}
            onChange={(e) => {
              if (isRgMode(e.target.value)) player.setRgMode(e.target.value)
            }}
          >
            <option value="off">off</option>
            <option value="track">track</option>
            <option value="album">album</option>
          </select>
        </label>
        <label className="small" title="可逆を Derived の Opus ではなく原本で再生する">
          <input
            type="checkbox"
            checked={player.preferOriginal}
            onChange={(e) => player.setPreferOriginal(e.target.checked)}
          />
          原本
        </label>
        <span title="音量">🔊</span>
        <input
          type="range"
          className="volume"
          min={0}
          max={1}
          step={0.01}
          value={player.volume}
          onChange={(e) => player.setVolume(Number(e.target.value))}
        />
      </div>
      <div className="menu" ref={menuRef}>
        <button
          type="button"
          className={`ghost menu-button${menuOpen ? ' active' : ''}`}
          title="ジョブ / 履歴 / 設定 / ログアウト"
          aria-haspopup="menu"
          aria-expanded={menuOpen}
          onClick={() => setMenuOpen((o) => !o)}
        >
          ☰
          {summary && summary.failed > 0 ? <span className="jobs-failed" aria-label="ジョブに失敗あり" /> : null}
          <span className={`dot ${connected ? 'on' : 'off'}`} title={connected ? 'SSE 接続中' : 'SSE 切断（再接続中）'} />
        </button>
        {menuOpen && (
          <div className="menu-list" role="menu">
            {MENU_VIEWS.map(([v, label]) => (
              <button
                key={v}
                type="button"
                role="menuitem"
                className={v === view ? 'active' : ''}
                title={v === 'jobs' ? jobsTitle : undefined}
                onClick={() => {
                  onView(v)
                  setMenuOpen(false)
                }}
              >
                {label}
                {v === 'jobs' && active > 0 ? <span className="jobs-count">{formatCount(active)}</span> : null}
              </button>
            ))}
            <div className="menu-sep" />
            <button
              type="button"
              role="menuitem"
              onClick={() => {
                setMenuOpen(false)
                onLogout()
              }}
            >
              ログアウト
            </button>
          </div>
        )}
      </div>
    </header>
  )
}
