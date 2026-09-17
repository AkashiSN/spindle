// アルバムのサムネイルグリッド（SPEC §12.6、P1-3）。クリックで一覧を album_id に絞る。
// 画像は /api/artwork/:hash?size=256（ハッシュアドレス。サムネイル未生成のうちは原画像が返る）

import type { AlbumRow } from '../api/types'
import { albumTitle, artworkUrl, gridAlbums } from '../lib/artwork'
import { formatCount } from '../lib/format'

export function AlbumGrid({
  albums,
  error,
  onOpen,
}: {
  albums: AlbumRow[]
  error: string | null
  onOpen: (album: AlbumRow) => void
}) {
  const rows = gridAlbums(albums)
  return (
    <section className="album-grid-view">
      <header className="album-grid-head">
        <h1>アルバム</h1>
        <span className="muted">{formatCount(rows.length)} 件</span>
        {error ? <span className="error">{error}</span> : null}
      </header>
      {rows.length === 0 ? (
        <p className="muted album-grid-empty">アルバムがありません。スキャンを実行してください</p>
      ) : (
        <ul className="album-grid">
          {rows.map((a) => (
            <li key={a.id}>
              <button
                type="button"
                className="album-card"
                title={`${a.albumartist ?? ''} / ${albumTitle(a)}`}
                onClick={() => onOpen(a)}
              >
                {a.artwork_hash ? (
                  <img
                    className="album-art"
                    src={artworkUrl(a.artwork_hash, 256)}
                    alt=""
                    loading="lazy"
                    decoding="async"
                  />
                ) : (
                  <span className="album-art album-art-none" aria-hidden="true">
                    ♪
                  </span>
                )}
                <span className="album-title">{albumTitle(a)}</span>
                <span className="album-artist muted">{a.albumartist ?? '（アルバムアーティストなし）'}</span>
                <span className="album-meta muted">
                  {a.date ? `${a.date} · ` : ''}
                  {formatCount(a.track_count)} 曲
                </span>
              </button>
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}
