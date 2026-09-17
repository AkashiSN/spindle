// 下部バー（SPEC §12.1）: 左が再生（P1-9、D-52）、右がジョブ要約「実行中 N · 反映待ち M · 失敗 K」

import type { JobSummary } from '../api/types'
import type { PlayerHandle } from '../hooks/usePlayer'
import { formatCount } from '../lib/format'
import { formatTime } from '../lib/playback'

export function BottomBar({
  player,
  summary,
  connected,
  onJobsClick,
}: {
  player: PlayerHandle
  summary: JobSummary | null
  connected: boolean
  onJobsClick: () => void
}) {
  const { track, playing, position, duration } = player
  const max = duration ?? (track?.duration_ms != null ? track.duration_ms / 1000 : 0)
  return (
    <footer className="bottom-bar">
      <div className="player">
        <button
          type="button"
          className="ghost play-toggle"
          disabled={!track}
          onClick={player.toggle}
          title={playing ? '一時停止' : '再生'}
        >
          {playing ? '‖' : '▶'}
        </button>
        <button type="button" className="ghost" disabled={!track} onClick={player.stop} title="停止">
          ■
        </button>
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
        <span className="time muted small">
          {formatTime(position)} / {formatTime(max)}
        </span>
        <label className="small" title="ReplayGain（track gain）を掛ける">
          <input type="checkbox" checked={player.rgEnabled} onChange={(e) => player.setRgEnabled(e.target.checked)} />
          RG
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
        <span className="now-playing" title={track?.rel_path}>
          {player.error ? (
            <span className="error">{player.error}</span>
          ) : track ? (
            <>
              {track.title ?? track.rel_path}
              {track.artist_display ? <span className="muted"> — {track.artist_display}</span> : null}
              {player.transcoding ? <span className="muted small"> （変換中）</span> : null}
            </>
          ) : (
            <span className="muted small">行の ▶ で再生</span>
          )}
        </span>
      </div>
      <button type="button" className="ghost jobs-summary" onClick={onJobsClick} title="ジョブ画面へ">
        {summary ? (
          <>
            実行中 {formatCount(summary.running)} · 反映待ち {formatCount(summary.pending_ops)} · 失敗{' '}
            <span className={summary.failed > 0 ? 'error' : ''}>{formatCount(summary.failed)}</span>
            {summary.queued > 0 ? <span className="muted"> · 待ち {formatCount(summary.queued)}</span> : null}
          </>
        ) : (
          <span className="muted">ジョブ要約を取得中…</span>
        )}
        <span className={`dot ${connected ? 'on' : 'off'}`} title={connected ? 'SSE 接続中' : 'SSE 切断（再接続中）'} />
      </button>
    </footer>
  )
}
