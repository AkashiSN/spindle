// 左下のアルバムアート（P1-12、D-58）。選択行（先頭）のアルバム、無ければ再生中のアルバムの
// 768px WebP を正方形に収める。画像が無ければ空のまま

import type { AlbumRow } from '../api/types'
import { artworkUrl } from '../lib/artwork'

export function AlbumArt({ album }: { album: AlbumRow | null }) {
  const hash = album?.artwork_hash ?? null
  return (
    <div className="album-art" title={album ? `${album.albumartist ?? ''} — ${album.album ?? album.rel_dir}` : undefined}>
      {hash ? (
        <img src={artworkUrl(hash, 768)} alt={album?.album ?? ''} />
      ) : (
        <div className="album-art-empty muted small">{album ? 'アートワークなし' : ''}</div>
      )}
    </div>
  )
}
