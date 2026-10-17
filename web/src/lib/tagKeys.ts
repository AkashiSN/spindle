// タグのキーの入力欄の候補（D-113）。ライブラリにあるキー（GET /api/tags/keys。曲数の多い順）に
// よく使うキーを足し、入力のたびに前方一致で絞る。キーは大文字で持つので大小文字を区別しない

export type TagKeyUsage = { key: string; tracks: number }

/** ライブラリのキー（並びはそのまま）の後ろに、ライブラリに無いよく使うキーを 0 曲で足す */
export function mergeTagKeys(library: readonly TagKeyUsage[], common: readonly string[]): TagKeyUsage[] {
  const seen = new Set(library.map((k) => k.key))
  return [...library, ...common.filter((k) => !seen.has(k)).map((key) => ({ key, tracks: 0 }))]
}

/**
 * 入力で前方一致する候補（並びは `keys` のまま）。空の入力では出さない。`exclude`（既にある列・フィールド）は除く。
 * 入力と完全に同じキーしか残らないときは出さない（選び終えている）
 */
export function filterTagKeys(
  keys: readonly TagKeyUsage[],
  input: string,
  opts: { exclude?: readonly string[]; limit?: number } = {},
): TagKeyUsage[] {
  const prefix = input.trim().toUpperCase()
  if (prefix === '') return []
  const exclude = new Set(opts.exclude ?? [])
  const hits = keys.filter((k) => k.key.startsWith(prefix) && !exclude.has(k.key))
  if (hits.length === 1 && hits[0].key === prefix) return []
  return hits.slice(0, opts.limit ?? 12)
}

/** 候補の選択位置を動かす。-1 は「選んでいない」（入力欄のまま）。端では止まる */
export function moveActive(active: number, count: number, step: 1 | -1): number {
  if (count === 0) return -1
  return Math.max(-1, Math.min(count - 1, active + step))
}
