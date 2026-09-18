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

/** 編集履歴 / track_tags の `PICTURE` 値 `<mime>:<sha256hex>` を分解する。形が違えば null */
export function parsePictureValue(v: unknown): { mime: string; hash: string } | null {
  if (typeof v !== 'string') return null
  const i = v.lastIndexOf(':')
  if (i <= 0) return null
  const mime = v.slice(0, i)
  const hash = v.slice(i + 1)
  if (!/^[0-9a-f]{64}$/.test(hash)) return null
  return { mime, hash }
}

/** アップロードできる画像（サーバの EMBED_MIMES と一致させる） */
export const EMBED_MIMES = ['image/jpeg', 'image/png', 'image/webp'] as const

export type UploadedArtwork = { sha256: string; mime: string; width: number; height: number; bytes: number }

/** アップロード結果の 1 行の説明（形式・寸法・サイズ） */
export function uploadedSummary(u: UploadedArtwork): string {
  const kind = u.mime.replace(/^image\//, '').toUpperCase()
  const kb = u.bytes >= 1024 * 1024 ? `${(u.bytes / (1024 * 1024)).toFixed(1)} MiB` : `${Math.ceil(u.bytes / 1024)} KiB`
  return `${kind} ${u.width}×${u.height} ${kb}`
}
