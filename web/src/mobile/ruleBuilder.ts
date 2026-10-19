// スマートプレイリストの組み立て画面のモデル（docs/DSL.md）。
// - astToModel: サーバの AST（POST /api/playlists/preview の ast）を組み立て画面の形にする。
//   表せない式（括弧の入れ子、AND と OR の混在、NOT の下の and / or / present / missing）は null
//   → ルール文の画面で開く
// - modelToDsl: 組み立て画面の形を DSL の文にする（保存・件数の表示に使う）

export type CmpOp = 'is' | 'has' | 'greater' | 'less' | 'matches'
export type Condition =
  | { kind: 'cmp'; not: boolean; field: string; cmp: CmpOp; value: string }
  | { kind: 'present' | 'missing'; field: string } // NOT は付けない（MISSING / PRESENT の言い換えで足りる）
export type BuilderModel = {
  join: 'and' | 'or'
  conditions: Condition[] // 1 つ以上
  order: { field: string; desc: boolean } | { field: 'random' } | null
  limit: number | null
}

export const CMP_OPS: readonly CmpOp[] = ['is', 'has', 'greater', 'less', 'matches']

/** DSL.md の標準タグ + 拡張フィールド（端末のフィールド on_device / device_pending は除く）。任意のタグ名は自由入力 */
export const FIELD_CHOICES: readonly string[] = [
  'title',
  'artist',
  'albumartist',
  'album',
  'date',
  'genre',
  'tracknumber',
  'discnumber',
  'composer',
  'comment',
  'category',
  'verification',
  'source_type',
  'lossless',
  'codec',
  'samplerate',
  'bitdepth',
  'channels',
  'bitrate',
  'duration',
  'added',
  'has_derived',
  'missing',
  'hirescheck',
  'cutoff',
  'cliff',
  'effectivebits',
]

/** 値に `"` がある（組み立て画面からは保存させず、ルール文の画面へ移るよう促す） */
export class QuoteInValue extends Error {
  readonly value: string
  constructor(value: string) {
    super('値に " を含む条件は組み立て画面では書けません。ルール文で編集してください')
    this.name = 'QuoteInValue'
    this.value = value
  }
}

/** フィールド名に使えない文字がある（文法の ident は英数字・_・空白だけ） */
export class InvalidField extends Error {
  readonly field: string
  constructor(field: string) {
    super(`フィールド名「${field}」は使えません（英数字と _ だけ）`)
    this.name = 'InvalidField'
    this.field = field
  }
}

/** フィールド名として書けるか（`%…%` の中身。空白だけは不可） */
export function isValidField(f: string): boolean {
  return /^[A-Za-z0-9_ ]+$/.test(f) && f.trim() !== ''
}

// ---------------------------------------------------------------- AST → モデル

type Obj = Record<string, unknown>

function isObj(v: unknown): v is Obj {
  return v != null && typeof v === 'object' && !Array.isArray(v)
}

function isCmpOp(v: unknown): v is CmpOp {
  return typeof v === 'string' && (CMP_OPS as readonly string[]).includes(v)
}

function cmpOf(e: Obj, not: boolean): Condition | null {
  if (typeof e.field !== 'string' || !isCmpOp(e.cmp) || typeof e.value !== 'string') return null
  return { kind: 'cmp', not, field: e.field, cmp: e.cmp, value: e.value }
}

/** 1 行の条件（cmp / NOT cmp / present / missing）。それ以外は null */
function conditionOf(e: unknown): Condition | null {
  if (!isObj(e)) return null
  switch (e.op) {
    case 'cmp':
      return cmpOf(e, false)
    case 'present':
    case 'missing':
      return typeof e.field === 'string' ? { kind: e.op, field: e.field } : null
    case 'not': {
      const args = e.args
      if (!Array.isArray(args) || args.length !== 1) return null
      const inner: unknown = args[0]
      return isObj(inner) && inner.op === 'cmp' ? cmpOf(inner, true) : null
    }
    default:
      return null
  }
}

function orderOf(o: unknown): BuilderModel['order'] | undefined {
  if (o == null) return null
  if (!isObj(o) || typeof o.field !== 'string') return undefined
  if (o.field.toLowerCase() === 'random') return { field: 'random' }
  if (o.dir == null || o.dir === 'asc') return { field: o.field, desc: false }
  if (o.dir === 'desc') return { field: o.field, desc: true }
  return undefined
}

function limitOf(l: unknown): number | null | undefined {
  if (l == null) return null
  return typeof l === 'number' && Number.isSafeInteger(l) && l >= 0 ? l : undefined
}

export function astToModel(ast: unknown): BuilderModel | null {
  if (!isObj(ast) || !isObj(ast.where)) return null
  const order = orderOf(ast.order)
  const limit = limitOf(ast.limit)
  if (order === undefined || limit === undefined) return null
  const w = ast.where
  let join: BuilderModel['join'] = 'and'
  let conditions: Condition[]
  if (w.op === 'and' || w.op === 'or') {
    if (!Array.isArray(w.args) || w.args.length === 0) return null
    join = w.op
    conditions = []
    for (const a of w.args as unknown[]) {
      const c = conditionOf(a)
      if (c == null) return null
      conditions.push(c)
    }
  } else {
    const c = conditionOf(w)
    if (c == null) return null
    conditions = [c]
  }
  return { join, conditions, order, limit }
}

// ---------------------------------------------------------------- モデル → DSL

/**
 * 値を DSL に書く形にする。空白（WHITESPACE）・`)` を含むか空なら二重引用符で囲む
 * （文法の bare は空白と `)` で終わる）。囲むときは `\` を `\\` にする（サーバは引用の中の `\\` を `\` と読む）。
 * `"` は組み立て画面では扱わない（QuoteInValue）
 */
export function quoteValue(v: string): string {
  if (v.includes('"')) throw new QuoteInValue(v)
  if (v === '' || /[\s)]/.test(v)) return `"${v.replace(/\\/g, '\\\\')}"`
  return v
}

function fieldRef(f: string): string {
  if (!isValidField(f)) throw new InvalidField(f)
  return `%${f.trim()}%`
}

function conditionToDsl(c: Condition): string {
  if (c.kind !== 'cmp') return `${c.kind.toUpperCase()} ${fieldRef(c.field)}`
  return `${c.not ? 'NOT ' : ''}${fieldRef(c.field)} ${c.cmp.toUpperCase()} ${quoteValue(c.value)}`
}

/** 組み立て画面の形を DSL の文にする。値に `"` があれば QuoteInValue、フィールド名が不正なら InvalidField */
export function modelToDsl(m: BuilderModel): string {
  const parts = [m.conditions.map(conditionToDsl).join(m.join === 'and' ? ' AND ' : ' OR ')]
  if (m.order != null) {
    if (!('desc' in m.order)) parts.push('ORDER BY random')
    else parts.push(`ORDER BY ${fieldRef(m.order.field)}${m.order.desc ? ' DESC' : ''}`)
  }
  if (m.limit != null) parts.push(`LIMIT ${m.limit}`)
  return parts.join(' ')
}

/** 「＋ 条件を追加」とスマートの作成で最初に置く条件 */
export const NEW_CONDITION: Condition = { kind: 'cmp', not: false, field: 'albumartist', cmp: 'is', value: '' }

/** モデルを DSL にする。できなければ画面に出す文言 */
export function dslOrError(m: BuilderModel): { dsl: string } | { error: string } {
  try {
    return { dsl: modelToDsl(m) }
  } catch (e) {
    if (e instanceof QuoteInValue)
      return { error: '値に " を含む条件は組み立て画面では書けません。「文で編集する」でルール文として書いてください' }
    return { error: e instanceof Error ? e.message : String(e) }
  }
}

// ---------------------------------------------------------------- ルール文の入力補助

/** 入力補助のボタン。close があれば選んだ範囲を囲む、無ければキーワード（前後に空白） */
export type Snippet = { label: string; open: string; close?: string }

export const SNIPPETS: readonly Snippet[] = [
  { label: '%…%', open: '%', close: '%' },
  { label: 'IS', open: 'IS' },
  { label: 'HAS', open: 'HAS' },
  { label: 'AND', open: 'AND' },
  { label: 'OR', open: 'OR' },
  { label: 'NOT', open: 'NOT' },
  { label: '( )', open: '(', close: ')' },
  { label: 'ORDER BY', open: 'ORDER BY' },
  { label: 'LIMIT', open: 'LIMIT' },
]

/** カーソル位置（start〜end の選択範囲）に入れた後の文と、カーソルの位置 */
export function insertSnippet(value: string, start: number, end: number, s: Snippet): { value: string; caret: number } {
  const head = value.slice(0, start)
  const sel = value.slice(start, end)
  const tail = value.slice(end)
  if (s.close != null) {
    const text = s.open + sel + s.close
    return { value: head + text + tail, caret: start + (sel === '' ? s.open.length : text.length) }
  }
  const lead = head === '' || /[\s(]$/.test(head) ? '' : ' '
  const text = `${lead}${s.open} `
  return { value: head + text + tail, caret: start + text.length }
}
