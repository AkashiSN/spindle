import { describe, expect, it } from 'vitest'
import {
  FIELD_CHOICES,
  InvalidField,
  SNIPPETS,
  astToModel,
  dslOrError,
  insertSnippet,
  isValidField,
  modelToDsl,
  quoteValue,
  type BuilderModel,
} from './ruleBuilder'

const cmp = (field: string, c: string, value: string) => ({ op: 'cmp', field, cmp: c, value })

describe('astToModel', () => {
  it('平らな AND（NOT 付き含む）と ORDER / LIMIT は組み立てられる', () => {
    const ast = {
      where: {
        op: 'and',
        args: [cmp('albumartist', 'is', 'ヰ世界情緒'), { op: 'not', args: [cmp('category', 'is', '_Unsorted')] }],
      },
      order: { field: 'date', dir: 'desc' },
      limit: 100,
    }
    expect(astToModel(ast)).toEqual({
      join: 'and',
      conditions: [
        { kind: 'cmp', not: false, field: 'albumartist', cmp: 'is', value: 'ヰ世界情緒' },
        { kind: 'cmp', not: true, field: 'category', cmp: 'is', value: '_Unsorted' },
      ],
      order: { field: 'date', desc: true },
      limit: 100,
    })
  })

  it('条件 1 つ（and / or で包まれない）と OR だけの式も組み立てられる', () => {
    expect(astToModel({ where: cmp('genre', 'has', 'Rock') })!.conditions).toHaveLength(1)
    expect(astToModel({ where: { op: 'or', args: [cmp('a', 'is', '1'), cmp('b', 'is', '2')] } })!.join).toBe('or')
  })

  it('入れ子・AND と OR の混在・NOT の下の and や present は null（ルール文で開く）', () => {
    expect(
      astToModel({
        where: { op: 'and', args: [cmp('a', 'is', '1'), { op: 'or', args: [cmp('b', 'is', '2'), cmp('c', 'is', '3')] }] },
      }),
    ).toBeNull()
    expect(
      astToModel({ where: { op: 'not', args: [{ op: 'and', args: [cmp('a', 'is', '1'), cmp('b', 'is', '2')] }] } }),
    ).toBeNull()
    expect(astToModel({ where: { op: 'not', args: [{ op: 'present', field: 'genre' }] } })).toBeNull()
    expect(astToModel({ nope: 1 })).toBeNull()
  })

  it('PRESENT / MISSING、ORDER の random と dir 省略（昇順）、ORDER / LIMIT 無し', () => {
    expect(
      astToModel({
        where: { op: 'and', args: [{ op: 'present', field: 'genre' }, { op: 'missing', field: 'comment' }] },
        order: { field: 'random' },
      }),
    ).toEqual({
      join: 'and',
      conditions: [
        { kind: 'present', field: 'genre' },
        { kind: 'missing', field: 'comment' },
      ],
      order: { field: 'random' },
      limit: null,
    })
    expect(astToModel({ where: cmp('a', 'is', '1'), order: { field: 'title' } })!.order).toEqual({
      field: 'title',
      desc: false,
    })
    expect(astToModel({ where: cmp('a', 'is', '1') })!.order).toBeNull()
  })

  it('形のおかしい AST は null', () => {
    expect(astToModel(null)).toBeNull()
    expect(astToModel({ where: cmp('a', 'eq', '1') })).toBeNull()
    expect(astToModel({ where: { op: 'cmp', field: 'a', cmp: 'is', value: 1 } })).toBeNull()
    expect(astToModel({ where: { op: 'and', args: [] } })).toBeNull()
    expect(astToModel({ where: { op: 'not', args: [cmp('a', 'is', '1'), cmp('b', 'is', '2')] } })).toBeNull()
    expect(astToModel({ where: { op: 'not', args: [{ op: 'not', args: [cmp('a', 'is', '1')] }] } })).toBeNull()
    expect(astToModel({ where: cmp('a', 'is', '1'), limit: -1 })).toBeNull()
    expect(astToModel({ where: cmp('a', 'is', '1'), order: { field: 'date', dir: 'up' } })).toBeNull()
  })
})

describe('modelToDsl', () => {
  it('modelToDsl は引用が要る値を二重引用符で囲み、往復で同じモデルになる', () => {
    const m: BuilderModel = {
      join: 'and',
      conditions: [
        { kind: 'cmp', not: false, field: 'albumartist', cmp: 'is', value: 'Official 髭男dism' },
        { kind: 'cmp', not: true, field: 'category', cmp: 'is', value: '_Unsorted' },
        { kind: 'missing', field: 'genre' },
      ],
      order: { field: 'random' },
      limit: 50,
    }
    expect(modelToDsl(m)).toBe(
      '%albumartist% IS "Official 髭男dism" AND NOT %category% IS _Unsorted AND MISSING %genre% ORDER BY random LIMIT 50',
    )
  })

  it('OR・PRESENT・演算子の大文字化・ORDER の降順 / 昇順', () => {
    const base: BuilderModel = {
      join: 'or',
      conditions: [
        { kind: 'cmp', not: false, field: 'samplerate', cmp: 'greater', value: '48000' },
        { kind: 'present', field: 'comment' },
      ],
      order: { field: 'date', desc: true },
      limit: null,
    }
    expect(modelToDsl(base)).toBe('%samplerate% GREATER 48000 OR PRESENT %comment% ORDER BY %date% DESC')
    expect(modelToDsl({ ...base, order: { field: 'date', desc: false } })).toBe(
      '%samplerate% GREATER 48000 OR PRESENT %comment% ORDER BY %date%',
    )
  })

  it('値の " と \\ は引用の中でエスケープする（サーバの quoted は \\" と \\\\ を読む）', () => {
    const m: BuilderModel = {
      join: 'and',
      conditions: [{ kind: 'cmp', not: false, field: 'title', cmp: 'is', value: 'a"b\\c' }],
      order: null,
      limit: null,
    }
    expect(modelToDsl(m)).toBe('%title% IS "a\\"b\\\\c"')
  })

  it('フィールド名に使えない文字があると InvalidField を投げる', () => {
    const m: BuilderModel = {
      join: 'and',
      conditions: [{ kind: 'present', field: 'a%b' }],
      order: null,
      limit: null,
    }
    expect(() => modelToDsl(m)).toThrow(InvalidField)
    expect(() => modelToDsl({ ...m, conditions: [{ kind: 'present', field: 'genre' }], order: { field: '', desc: false } })).toThrow(
      InvalidField,
    )
  })
})

describe('quoteValue', () => {
  it('空白・閉じ括弧・空文字は囲み、それ以外はそのまま', () => {
    expect(quoteValue('Rock')).toBe('Rock')
    expect(quoteValue('(笑)')).toBe('"(笑)"')
    expect(quoteValue('a(b')).toBe('a(b')
    expect(quoteValue('')).toBe('""')
    expect(quoteValue('a\tb')).toBe('"a\tb"')
    expect(quoteValue('全角　空白')).toBe('"全角　空白"')
  })
  it('" か \\ を含めば囲み、\\ → \\\\、" → \\" にする', () => {
    expect(quoteValue('a\\b')).toBe('"a\\\\b"')
    expect(quoteValue('a \\')).toBe('"a \\\\"')
    expect(quoteValue('"x')).toBe('"\\"x"')
    expect(quoteValue('say "hi"')).toBe('"say \\"hi\\""')
  })
})

describe('isValidField / FIELD_CHOICES', () => {
  it('英数字・_・空白だけを受け付ける', () => {
    expect(isValidField('albumartist')).toBe(true)
    expect(isValidField('album artist')).toBe(true)
    expect(isValidField('MUSICBRAINZ_ALBUMID')).toBe(true)
    expect(isValidField('')).toBe(false)
    expect(isValidField('   ')).toBe(false)
    expect(isValidField('タグ')).toBe(false)
    expect(isValidField('a%b')).toBe(false)
  })
  it('標準タグと拡張フィールドを持ち、端末のフィールドは含めない', () => {
    for (const f of ['title', 'albumartist', 'comment', 'category', 'verification', 'effectivebits']) {
      expect(FIELD_CHOICES).toContain(f)
    }
    expect(FIELD_CHOICES).not.toContain('on_device')
    expect(FIELD_CHOICES).not.toContain('device_pending')
  })
})

describe('insertSnippet', () => {
  const snip = (label: string) => SNIPPETS.find((s) => s.label === label)!
  it('キーワードは前に空白が無ければ足し、後ろに空白を付ける', () => {
    expect(insertSnippet('%a%', 3, 3, snip('IS'))).toEqual({ value: '%a% IS ', caret: 7 })
    expect(insertSnippet('%a% ', 4, 4, snip('IS'))).toEqual({ value: '%a% IS ', caret: 7 })
    expect(insertSnippet('', 0, 0, snip('NOT'))).toEqual({ value: 'NOT ', caret: 4 })
    expect(insertSnippet('x y', 1, 3, snip('AND'))).toEqual({ value: 'x AND ', caret: 6 })
  })
  it('%…% と ( ) は選んだ範囲を囲む。選んでいなければ中にカーソルを置く', () => {
    expect(insertSnippet('IS ', 0, 0, snip('%…%'))).toEqual({ value: '%%IS ', caret: 1 })
    expect(insertSnippet('genre', 0, 5, snip('%…%'))).toEqual({ value: '%genre%', caret: 7 })
    expect(insertSnippet('a OR b', 0, 6, snip('( )'))).toEqual({ value: '(a OR b)', caret: 8 })
  })
  it('ボタンの並び', () => {
    expect(SNIPPETS.map((s) => s.label)).toEqual(['%…%', 'IS', 'HAS', 'AND', 'OR', 'NOT', '( )', 'ORDER BY', 'LIMIT'])
  })
})

describe('dslOrError', () => {
  it('正しければ文、フィールド名が不正なら文言', () => {
    const m: BuilderModel = { join: 'and', conditions: [{ kind: 'present', field: 'genre' }], order: null, limit: null }
    expect(dslOrError(m)).toEqual({ dsl: 'PRESENT %genre%' })
    const bad = dslOrError({ ...m, conditions: [{ kind: 'present', field: 'a%b' }] })
    expect('error' in bad && bad.error).toContain('a%b')
  })
})
