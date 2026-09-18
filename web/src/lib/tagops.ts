// 一括編集の操作リスト（SPEC §12.3、D-42）。サーバの `domain::tagops` の JSON 形と 1 対 1。
// クライアント側の `id` は React の key と並べ替えのためだけで、送るときに落とす

export type OpKind = 'set' | 'ref' | 'replace' | 'number' | 'delete'

export type TagOp =
  | { id: string; op: 'set'; key: string; value: string }
  | { id: string; op: 'ref'; key: string; template: string }
  | { id: string; op: 'replace'; key: string; pattern: string; replacement: string }
  | { id: string; op: 'number'; key: string; start: number; pad: number }
  | { id: string; op: 'delete'; key: string }

export const OP_KINDS: OpKind[] = ['set', 'ref', 'replace', 'number', 'delete']

export const OP_LABELS: Record<OpKind, string> = {
  set: '固定値',
  ref: 'フィールド参照',
  replace: '正規表現置換',
  number: '連番',
  delete: '削除',
}

/** よく使うキー（datalist の候補。任意のキーも入力できる） */
export const COMMON_KEYS = [
  'TITLE',
  'ARTIST',
  'ALBUM',
  'ALBUMARTIST',
  'DATE',
  'GENRE',
  'TRACKNUMBER',
  'DISCNUMBER',
  'TRACKTOTAL',
  'DISCTOTAL',
  'COMMENT',
  'COMPOSER',
  'LABEL',
  'MUSICBRAINZ_ALBUMID',
  'MUSICBRAINZ_TRACKID',
]

let seq = 0
function nextId(): string {
  seq += 1
  return `op${seq}-${Date.now().toString(36)}`
}

export function newOp(kind: OpKind): TagOp {
  const id = nextId()
  switch (kind) {
    case 'set':
      return { id, op: 'set', key: '', value: '' }
    case 'ref':
      return { id, op: 'ref', key: '', template: '' }
    case 'replace':
      return { id, op: 'replace', key: '', pattern: '', replacement: '' }
    case 'number':
      return { id, op: 'number', key: 'TRACKNUMBER', start: 1, pad: 0 }
    case 'delete':
      return { id, op: 'delete', key: '' }
  }
}

/** サーバへ送る形（`domain::tagops::parse_ops` が受ける JSON） */
export type OpRequest =
  /** 値は文字列か多値の配列（サーバはどちらも受け、空要素を落とす） */
  | { op: 'set'; key: string; value: string | string[] }
  | { op: 'ref'; key: string; template: string }
  | { op: 'replace'; key: string; pattern: string; replacement: string }
  | { op: 'number'; key: string; start: number; pad: number }
  | { op: 'delete'; key: string }

function normKey(key: string): string {
  return key.trim().toUpperCase()
}

export function opsToRequest(ops: readonly TagOp[]): OpRequest[] {
  return ops.map((o) => {
    switch (o.op) {
      case 'set':
        return { op: 'set', key: normKey(o.key), value: o.value }
      case 'ref':
        return { op: 'ref', key: normKey(o.key), template: o.template }
      case 'replace':
        return { op: 'replace', key: normKey(o.key), pattern: o.pattern, replacement: o.replacement }
      case 'number':
        return { op: 'number', key: normKey(o.key), start: o.start, pad: o.pad }
      case 'delete':
        return { op: 'delete', key: normKey(o.key) }
    }
  })
}

const KEY_RE = /^[ -<>-}]+$/

/** 送る前の検証。最初の問題を返す（サーバでも検証するが、往復せずに直せるものはここで） */
export function validateOps(ops: readonly TagOp[]): string | null {
  if (ops.length === 0) return '操作を 1 つ以上追加してください'
  for (const [i, o] of ops.entries()) {
    const n = i + 1
    const key = normKey(o.key)
    if (!key || !KEY_RE.test(key)) return `${n}: キーが空か、使えない文字（= や制御文字）を含んでいます`
    if (key === 'PICTURE') return `${n}: 画像はタグ編集の対象外です`
    if (o.op === 'replace' && o.pattern === '') return `${n}: パターンが空です`
    if (o.op === 'number' && (!Number.isInteger(o.start) || o.start < 0)) return `${n}: 開始は 0 以上の整数です`
    if (o.op === 'number' && (!Number.isInteger(o.pad) || o.pad < 0 || o.pad > 6)) return `${n}: 桁は 0 〜 6 の整数です`
  }
  return null
}

/** `id` の操作を `delta`（-1 / +1）だけ動かす。動かせなければ同じ配列を返す */
export function moveOp(ops: readonly TagOp[], id: string, delta: -1 | 1): readonly TagOp[] {
  const i = ops.findIndex((o) => o.id === id)
  const j = i + delta
  if (i < 0 || j < 0 || j >= ops.length) return ops
  const out = [...ops]
  const [it] = out.splice(i, 1)
  out.splice(j, 0, it!)
  return out
}
