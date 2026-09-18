// GET /api/tracks の filter（SPEC §9、D-39）。サーバのホワイトリストと同じキーだけを持つ。
// サーバへは URL エンコードした JSON 1 文字列で渡し、selection.filter にも同じ文字列を使う

export const FLAGS = [
  'unverified',
  'duplicate',
  'missing',
  'no_rg',
  'rg_unwritten',
  'pending',
  'conflict',
  'hardlink',
  'flac_unchecked',
  'flac_error',
] as const
export type Flag = (typeof FLAGS)[number]

export const FLAG_LABELS: Record<Flag, string> = {
  unverified: '未検証',
  duplicate: '重複',
  missing: 'missing',
  no_rg: 'RG なし',
  rg_unwritten: 'RG 未書込',
  pending: '反映待ち',
  conflict: 'conflict',
  hardlink: 'hardlink',
  flac_unchecked: 'FLAC 未検査',
  flac_error: 'FLAC エラー',
}

export type Filter = {
  category?: string
  albumartist?: string
  album_id?: number
  /** ツリーのノード配下の album id（D-58）。空配列は空集合 */
  album_ids?: number[]
  playlist_id?: number
  flags?: Flag[]
  /** スマートプレイリスト DSL（WHERE だけ。ルール編集中のプレビュー用。D-39 / D-54） */
  dsl?: string
  q?: string
}

export const EMPTY_FILTER: Filter = {}

/** 空の値を落とし、キー順と flags の順を固定した JSON。同じ集合なら同じ文字列になる */
export function filterToParam(f: Filter): string {
  const out: Record<string, unknown> = {}
  if (f.category) out.category = f.category
  if (f.albumartist) out.albumartist = f.albumartist
  if (f.album_id != null) out.album_id = f.album_id
  if (f.album_ids) out.album_ids = [...new Set(f.album_ids)].sort((a, b) => a - b)
  if (f.playlist_id != null) out.playlist_id = f.playlist_id
  const flags = [...new Set(f.flags ?? [])].sort()
  if (flags.length > 0) out.flags = flags
  const dsl = f.dsl?.trim()
  if (dsl) out.dsl = dsl
  const q = f.q?.trim()
  if (q) out.q = q
  return Object.keys(out).length === 0 ? '' : JSON.stringify(out)
}

export function isEmptyFilter(f: Filter): boolean {
  return filterToParam(f) === ''
}

export function sameFilter(a: Filter, b: Filter): boolean {
  return filterToParam(a) === filterToParam(b)
}

// ---------------------------------------------------------------- ソート

export const SORT_KEYS = [
  'album',
  'title',
  'artist',
  'album_title',
  'albumartist',
  'date',
  'duration',
  'codec',
  'rel_path',
  'id',
  // プレイリスト内の並び。filter.playlist_id と組でだけ有効（lib/playlists の sortForScope が切り替える）
  'position',
] as const
export type SortKey = (typeof SORT_KEYS)[number]

export type Sort = { key: SortKey; desc: boolean }
export const DEFAULT_SORT: Sort = { key: 'album', desc: false }

export function sortToParam(s: Sort): string {
  return s.desc ? `-${s.key}` : s.key
}

/** 同じ列をクリックしたら向きを反転、別の列なら昇順で切り替える */
export function toggleSort(current: Sort, key: SortKey): Sort {
  if (current.key === key) return { key, desc: !current.desc }
  return { key, desc: false }
}

// ---------------------------------------------------------------- クエリ文字列

export const PAGE_LIMIT = 500

export function tracksUrl(opts: {
  filter: Filter
  sort: Sort
  cursor?: string | null
  limit?: number
}): string {
  const p = new URLSearchParams()
  const f = filterToParam(opts.filter)
  if (f) p.set('filter', f)
  p.set('sort', sortToParam(opts.sort))
  p.set('limit', String(opts.limit ?? PAGE_LIMIT))
  if (opts.cursor) p.set('cursor', opts.cursor)
  return `/api/tracks?${p.toString()}`
}
