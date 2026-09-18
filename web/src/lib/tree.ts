// サイドバーのツリー（SPEC §12.1、D-58）。foobar2000 の Album List と同じく表示形式をパターンで
// 定義し、`GET /api/albums` の行から組む。
//
// パターン: `|` が階層、`%field%` が差し込み、`[ … ]` は中のフィールドが全部空なら丸ごと省く
// （角括弧自体は表示しない）。
// 例 `%category%|%albumartist%|%album%`（by category）、`[%year%] %albumartist% — %album%`。
// `%folder%` だけは特別で、パターン全体がそれだけのとき rel_dir の階層をそのまま展開する
// （foobar の by folder structure。深さはアルバムごとに違ってよい）
// ノードの絞り込みは配下の album id の集合（filter.album_ids）で行うので、どんなパターンでも
// サーバ側に写像は要らない

import type { AlbumRow } from '../api/types'

/** パターンで使えるフィールド（アルバム単位で持っている値だけ） */
export const TREE_FIELDS = [
  'category',
  'albumartist',
  'album',
  'date',
  'year',
  'original_date',
  'edition',
  'disc_count',
  'rel_dir',
  'folder',
] as const
export type TreeField = (typeof TREE_FIELDS)[number]

export type Token =
  | { kind: 'text'; text: string }
  | { kind: 'field'; field: TreeField }
  | { kind: 'optional'; tokens: Token[] }
export type Level = Token[]
export type Pattern = { levels: Level[] }
export type PatternError = { error: string }

export type TreePreset = { name: string; pattern: string }

/** 組み込みの表示形式。先頭が既定 */
export const PRESETS: readonly TreePreset[] = [
  { name: 'by folder structure', pattern: '%folder%' },
  { name: 'by category', pattern: '%category%|%albumartist%|%album%' },
  { name: 'by artist', pattern: '%albumartist%|%album%' },
  { name: 'by album', pattern: '%album% — %albumartist%' },
  { name: 'by year', pattern: '%year%|%album%' },
]

/** 空の値の見出し */
export const EMPTY_LABEL = '（なし）'

function isField(s: string): s is TreeField {
  return (TREE_FIELDS as readonly string[]).includes(s)
}

/** パターンを階層ごとのトークン列にする。構文エラーは位置付きの文言 */
export function parsePattern(src: string): Pattern | PatternError {
  if (src.trim() === '') return { error: 'パターンが空' }
  const levels: Level[] = []
  let i = 0
  const n = src.length
  // 1 階層を読む。`|` か終端で止まる。depth > 0 なら `]` で止まる
  const readTokens = (depth: number): Token[] | PatternError => {
    const out: Token[] = []
    let text = ''
    const flush = () => {
      if (text !== '') out.push({ kind: 'text', text })
      text = ''
    }
    while (i < n) {
      const c = src[i]
      if (c === '|' && depth === 0) break
      if (c === ']' && depth > 0) break
      if (c === '%') {
        const end = src.indexOf('%', i + 1)
        if (end < 0) return { error: `${i + 1} 文字目: % が閉じていない` }
        const name = src.slice(i + 1, end).trim().toLowerCase()
        if (!isField(name)) return { error: `${i + 1} 文字目: 不明なフィールド %${name}%` }
        flush()
        out.push({ kind: 'field', field: name })
        i = end + 1
        continue
      }
      if (c === '[') {
        flush()
        const start = i
        i += 1
        const inner = readTokens(depth + 1)
        if ('error' in inner) return inner
        if (i >= n || src[i] !== ']') return { error: `${start + 1} 文字目: [ が閉じていない` }
        i += 1
        // 角括弧は条件記号で表示文字ではない（foobar の title formatting と同じ）
        out.push({ kind: 'optional', tokens: inner })
        continue
      }
      if (c === ']') return { error: `${i + 1} 文字目: 対応する [ が無い ]` }
      text += c
      i += 1
    }
    flush()
    return out
  }
  while (true) {
    const level = readTokens(0)
    if ('error' in level) return level
    if (level.length === 0) return { error: `${i + 1} 文字目: 空の階層` }
    levels.push(level)
    if (i >= n) break
    // src[i] は '|'
    i += 1
    if (i >= n) return { error: `${i} 文字目: | の後が空` }
  }
  // %folder% は単独でだけ使える（階層数がアルバムごとに違うので他と組み合わせられない）
  const usesFolder = (tokens: Token[]): boolean =>
    tokens.some((t) => (t.kind === 'field' ? t.field === 'folder' : t.kind === 'optional' && usesFolder(t.tokens)))
  if (levels.some(usesFolder)) {
    const alone = levels.length === 1 && levels[0].length === 1 && levels[0][0].kind === 'field'
    if (!alone) return { error: '%folder% はパターン全体がそれだけのときにしか使えない' }
  }
  return { levels }
}

/** パターンが `%folder%` 単独か */
export function isFolderPattern(p: Pattern): boolean {
  return p.levels.length === 1 && p.levels[0].length === 1 && p.levels[0][0].kind === 'field' && p.levels[0][0].field === 'folder'
}

function fieldValue(a: AlbumRow, f: TreeField): string {
  switch (f) {
    case 'category':
      return a.category ?? ''
    case 'albumartist':
      return a.albumartist ?? ''
    case 'album':
      return a.album ?? ''
    case 'date':
      return a.date ?? ''
    case 'year': {
      const d = a.date ?? ''
      return /^\d{4}/.test(d) ? d.slice(0, 4) : ''
    }
    case 'original_date':
      return a.original_date ?? ''
    case 'edition':
      return a.edition ?? ''
    case 'disc_count':
      return a.disc_count == null ? '' : String(a.disc_count)
    case 'rel_dir':
    case 'folder':
      return a.rel_dir
  }
}

/** 1 階層のラベル。`[ … ]` は中のフィールドが全部空なら空文字 */
export function renderLevel(level: Level, a: AlbumRow): string {
  const render = (tokens: Token[]): { text: string; anyField: boolean; anyValue: boolean } => {
    let text = ''
    let anyField = false
    let anyValue = false
    for (const t of tokens) {
      if (t.kind === 'text') text += t.text
      else if (t.kind === 'field') {
        const v = fieldValue(a, t.field)
        anyField = true
        if (v !== '') anyValue = true
        text += v
      } else {
        const r = render(t.tokens)
        if (!r.anyField || r.anyValue) {
          text += r.text
          anyField ||= r.anyField
          anyValue ||= r.anyValue
        } else anyField = true
      }
    }
    return { text, anyField, anyValue }
  }
  return render(level).text
}

export type TreeNode = {
  /** 階層の値を辿った一意な文字列（開閉状態の保持に使う） */
  key: string
  label: string
  /** 配下のトラック数 */
  count: number
  /** 配下の album id（昇順） */
  albumIds: number[]
  children: TreeNode[]
}

const collator = new Intl.Collator('ja')

/** missing でないアルバムをパターンで階層化する */
export function buildTree(albums: AlbumRow[], pattern: Pattern): TreeNode[] {
  type Work = { label: string; count: number; albumIds: number[]; children: Map<string, Work> }
  const root: Map<string, Work> = new Map()
  const folder = isFolderPattern(pattern)
  for (const a of albums) {
    if (a.missing_since != null) continue
    let level = root
    let labels = folder ? a.rel_dir.split('/').filter((s) => s !== '') : pattern.levels.map((l) => renderLevel(l, a))
    // Library 直下（rel_dir が空）のアルバムも 1 ノードに載せる（消すと All Music の件数と合わない）
    if (labels.length === 0) labels = [EMPTY_LABEL]
    for (const raw of labels) {
      const label = raw.trim() === '' ? EMPTY_LABEL : raw
      let w = level.get(label)
      if (!w) level.set(label, (w = { label, count: 0, albumIds: [], children: new Map() }))
      w.count += a.track_count
      w.albumIds.push(a.id)
      level = w.children
    }
  }
  const finish = (m: Map<string, Work>, prefix: string): TreeNode[] =>
    [...m.values()]
      // 「（なし）」は末尾
      .sort((x, y) =>
        x.label === EMPTY_LABEL ? 1 : y.label === EMPTY_LABEL ? -1 : collator.compare(x.label, y.label),
      )
      .map((w) => {
        const key = `${prefix}/${w.label}`
        return {
          key,
          label: w.label,
          count: w.count,
          albumIds: [...w.albumIds].sort((p, q) => p - q),
          children: finish(w.children, key),
        }
      })
  return finish(root, '')
}
