// アートワークの URL とグリッドの並び（SPEC §9 GET /api/artwork/:hash?size=、§12.6、P1-3）

import type { AlbumRow } from '../api/types'

/** サーバが持つサムネイルの一辺（media::artwork::THUMB_SIZES と一致させる） */
export const THUMB_SIZES = [256, 768] as const
export type ThumbSize = (typeof THUMB_SIZES)[number]

/** ハッシュアドレスの画像 URL。size 無しは原画像 */
export function artworkUrl(hash: string, size?: ThumbSize): string {
  const base = `/api/artwork/${encodeURIComponent(hash)}`
  return size == null ? base : `${base}?size=${size}`
}

/** グリッドに出す album（missing は出さない）。API の順（albumartist, album, id）を保つ */
export function gridAlbums(albums: AlbumRow[]): AlbumRow[] {
  return albums.filter((a) => a.missing_since == null)
}

/** グリッドの見出し行。album 名が無ければディレクトリ名 */
export function albumTitle(a: AlbumRow): string {
  if (a.album) return a.album
  const last = a.rel_dir.split('/').filter(Boolean).pop()
  return last ?? a.rel_dir
}
