// スマホ版のライブラリ（アーティスト / アルバム / 曲）の純粋なロジック。
// アーティストは albumartist（PC のツリーと同じ）で、GET /api/albums からクライアント側で組む

import type { AlbumRow } from '../api/types'
import { PAGE_LIMIT } from '../lib/filter'
import { loadJson, saveJson } from '../lib/storage'
import { EMPTY_LABEL } from '../lib/tree'
import type { LibraryView } from './route'

export const LIBRARY_VIEW_KEY = 'mobile.libraryView'

function isLibraryView(v: unknown): v is LibraryView {
  return v === 'artists' || v === 'albums' || v === 'tracks'
}

/** 最後に選んだ見方。既定は 'albums' */
export function loadLibraryView(): LibraryView {
  return loadJson<LibraryView>(LIBRARY_VIEW_KEY, 'albums', isLibraryView)
}

export function saveLibraryView(v: LibraryView): void {
  saveJson(LIBRARY_VIEW_KEY, v)
}

export type ArtistEntry = { name: string; albumCount: number; trackCount: number }

const collator = new Intl.Collator('ja')

/** アルバムの albumartist の表示名。無ければ（なし） */
export function artistName(a: Pick<AlbumRow, 'albumartist'>): string {
  return a.albumartist ? a.albumartist : EMPTY_LABEL
}

/** 大小を無視した部分一致。空の検索語はすべてに一致 */
function matches(hay: string | null, q: string): boolean {
  return hay != null && hay.toLocaleLowerCase().includes(q)
}

function normQuery(q: string | undefined): string {
  return (q ?? '').trim().toLocaleLowerCase()
}

/** albumartist ごとのアルバム数・曲数。missing を除き、検索語（大小無視の部分一致）で絞り、ja の照合順に並べる */
export function artistsOf(albums: AlbumRow[], q: string): ArtistEntry[] {
  const nq = normQuery(q)
  const by = new Map<string, ArtistEntry>()
  for (const a of albums) {
    if (a.missing_since != null) continue
    const name = artistName(a)
    if (nq !== '' && !matches(name, nq)) continue
    const e = by.get(name)
    if (e) {
      e.albumCount += 1
      e.trackCount += a.track_count
    } else {
      by.set(name, { name, albumCount: 1, trackCount: a.track_count })
    }
  }
  return [...by.values()].sort((x, y) => collator.compare(x.name, y.name))
}

/**
 * missing を除いたアルバム。`albumartist` は表示名（（なし）は albumartist の無いアルバム）で完全一致、
 * `q` はアルバム名・albumartist の大小無視の部分一致。並びは API の順を保つ
 */
export function albumsOf(albums: AlbumRow[], opts: { albumartist?: string; q?: string }): AlbumRow[] {
  const nq = normQuery(opts.q)
  return albums.filter(
    (a) =>
      a.missing_since == null &&
      (opts.albumartist == null || artistName(a) === opts.albumartist) &&
      (nq === '' || matches(a.album, nq) || matches(a.albumartist, nq)),
  )
}

/** 継ぎ足しの先読みを始める距離（行） */
const PREFETCH_ROWS = 50

/** 見えている最後の行から読み込み済みの末尾までが 50 行以内で、続きがあるときだけ次のページを読む */
export function nextPageNeeded(lastVisibleIndex: number, loaded: number, hasMore: boolean): boolean {
  return hasMore && loaded - 1 - lastVisibleIndex <= PREFETCH_ROWS
}

/** `GET /api/tracks` の `limit` の上限（サーバの MAX_LIMIT。超えると丸められる） */
export const SERVER_MAX_LIMIT = 1000

/**
 * SSE で取り直すときに読む各ページの `limit`。読み込み済みの行数（最低 1 ページ分）を覆うまで
 * サーバの上限で区切る。一覧が縮まないのでスクロール位置が保たれる
 */
export function refetchLimits(loaded: number): number[] {
  let rest = Math.max(PAGE_LIMIT, loaded)
  const out: number[] = []
  while (rest > 0) {
    const n = Math.min(rest, SERVER_MAX_LIMIT)
    out.push(n)
    rest -= n
  }
  return out
}
