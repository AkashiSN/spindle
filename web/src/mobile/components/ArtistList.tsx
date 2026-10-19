// アーティスト（albumartist）の一覧。タップでその albumartist のアルバム一覧へ

import { formatCount } from '../../lib/format'
import type { ArtistEntry } from '../library'

export function ArtistList({ artists, onOpen }: { artists: ArtistEntry[]; onOpen: (name: string) => void }) {
  if (artists.length === 0) return <p className="m-placeholder">該当するアーティストはありません</p>
  return (
    <ul className="m-list">
      {artists.map((a) => (
        <li key={a.name}>
          <button type="button" className="m-row" onClick={() => onOpen(a.name)}>
            <span className="m-row-main">
              <span className="m-row-title">{a.name}</span>
              <span className="m-row-sub">
                {formatCount(a.albumCount)} アルバム · {formatCount(a.trackCount)} 曲
              </span>
            </span>
            <span className="m-chevron" aria-hidden>
              ›
            </span>
          </button>
        </li>
      ))}
    </ul>
  )
}
