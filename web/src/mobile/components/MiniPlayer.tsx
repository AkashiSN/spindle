// 試し聴き中の曲（下タブの上に小さく）。タップでその曲のシートを開く。再生を始めてから出す

import type { TrackRow } from '../../api/types'
import { artworkUrl } from '../../lib/artwork'
import type { PreviewPlayer } from '../usePreviewPlayer'

export function MiniPlayer({ player, onOpen }: { player: PreviewPlayer; onOpen: (row: TrackRow) => void }) {
  const t = player.current
  if (!t) return null
  const title = t.title || (t.rel_path.split('/').pop() ?? t.rel_path)
  return (
    <div className="m-mini">
      <button type="button" className="m-mini-main" onClick={() => onOpen(t)}>
        {t.artwork_hash ? (
          <img className="m-mini-art" src={artworkUrl(t.artwork_hash, 256)} alt="" decoding="async" />
        ) : (
          <span className="m-mini-art m-art-empty" aria-hidden />
        )}
        <span className="m-row-main">
          <span className="m-row-title">{title}</span>
          {player.error != null ? (
            <span className="m-row-sub m-error">{player.error}</span>
          ) : (
            t.artist_display && <span className="m-row-sub">{t.artist_display}</span>
          )}
        </span>
      </button>
      <button type="button" className="m-icon m-mini-toggle" aria-label={player.playing ? '一時停止' : '再生'} onClick={player.toggle}>
        {player.playing ? '⏸' : '▶'}
      </button>
    </div>
  )
}
