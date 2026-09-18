// 左下のアートワーク（P1-12、D-58 / D-61）。選択行（先頭）、無ければ再生中のトラックについて、
// トラック自身の埋め込み画像 → 無ければ album の画像を 768px WebP で正方形に収める。画像が無ければ空のまま

import type { AlbumRow, TrackRow } from '../api/types'
import { artworkUrl, displayArtworkHash } from '../lib/artwork'

export function AlbumArt({ track, album }: { track: TrackRow | null; album: AlbumRow | null }) {
  const hash = displayArtworkHash(track, album)
  const title = album ? `${album.albumartist ?? ''} — ${album.album ?? album.rel_dir}` : (track?.title ?? undefined)
  return (
    <div className="album-art" title={title}>
      {hash ? (
        <img src={artworkUrl(hash, 768)} alt={album?.album ?? track?.title ?? ''} />
      ) : (
        <div className="album-art-empty muted small">{album || track ? 'アートワークなし' : ''}</div>
      )}
    </div>
  )
}
