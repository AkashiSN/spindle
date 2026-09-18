// プレイヤーバー（SPEC §12.1、P1-9 / P1-12、D-52 / D-58）。foobar2000 と同じく本体の上に置く:
// 左に曲名（無ければ Not playing）、中央に ◀◀ ▶ ▶▶ とシーク、右に RG / 原本 / 音量

import type { PlayerHandle } from '../hooks/usePlayer'
import { formatTime, isRgMode } from '../lib/playback'

export function PlayerBar({ player }: { player: PlayerHandle }) {
  const { track, playing, position, duration } = player
  const max = duration ?? (track?.duration_ms != null ? track.duration_ms / 1000 : 0)
  return (
    <div className="player-bar">
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
    </div>
  )
}
