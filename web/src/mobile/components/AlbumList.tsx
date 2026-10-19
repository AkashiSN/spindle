// アルバムの一覧。行にジャケット・アルバム名・albumartist・曲数。タップでアルバムの曲へ

import type { AlbumRow } from '../../api/types'
import { albumTitle, artworkUrl } from '../../lib/artwork'
import { formatCount } from '../../lib/format'
import { artistName } from '../library'

export function AlbumList({ albums, onOpen }: { albums: AlbumRow[]; onOpen: (a: AlbumRow) => void }) {
  if (albums.length === 0) return <p className="m-placeholder">該当するアルバムはありません</p>
  return (
    <ul className="m-list">
      {albums.map((a) => (
        <li key={a.id}>
          <button type="button" className="m-row m-album-row" onClick={() => onOpen(a)}>
            <AlbumArt hash={a.artwork_hash} />
            <span className="m-row-main">
              <span className="m-row-title">{albumTitle(a)}</span>
              <span className="m-row-sub">
                {artistName(a)} · {formatCount(a.track_count)} 曲
              </span>
            </span>
          </button>
        </li>
      ))}
    </ul>
  )
}

export function AlbumArt({ hash }: { hash: string | null }) {
  if (hash == null) return <span className="m-art m-art-empty" aria-hidden />
  return <img className="m-art" src={artworkUrl(hash, 256)} alt="" loading="lazy" decoding="async" />
}
