// 操作タブの album gain 切り替え（D-74）。選択行が属する album を一覧から引く

import type { AlbumRow } from '../api/types'

/** 一度に切り替えを出す album の上限（超えたら絞るよう促す） */
export const ALBUM_GAIN_LIMIT = 20

/** 選択行の album（出現順・重複なし。一覧に無い id と album 無しの行は落とす） */
export function albumsOfRows(
  rows: ReadonlyArray<{ album_id: number | null }>,
  albums: ReadonlyArray<AlbumRow>,
): AlbumRow[] {
  const byId = new Map(albums.map((a) => [a.id, a]))
  const out: AlbumRow[] = []
  const seen = new Set<number>()
  for (const r of rows) {
    if (r.album_id == null || seen.has(r.album_id)) continue
    seen.add(r.album_id)
    const a = byId.get(r.album_id)
    if (a != null) out.push(a)
  }
  return out
}
