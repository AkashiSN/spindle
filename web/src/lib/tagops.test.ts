import { describe, expect, it } from 'vitest'
import {
  newOp,
  opsToRequest,
  validateOps,
  moveOp,
  type TagOp,
} from './tagops'

describe('tagops', () => {
  it('newOp gives sensible defaults per kind', () => {
    expect(newOp('set')).toMatchObject({ op: 'set', key: '', value: '' })
    expect(newOp('ref')).toMatchObject({ op: 'ref', key: '', template: '' })
    expect(newOp('replace')).toMatchObject({ op: 'replace', key: '', pattern: '', replacement: '' })
    expect(newOp('number')).toMatchObject({ op: 'number', key: 'TRACKNUMBER', start: 1, pad: 0 })
    expect(newOp('delete')).toMatchObject({ op: 'delete', key: '' })
    expect(newOp('set').id).not.toBe(newOp('set').id)
  })

  it('opsToRequest strips client ids and uppercases keys', () => {
    const ops: TagOp[] = [
      { id: 'a', op: 'set', key: 'title', value: 'x' },
      { id: 'b', op: 'ref', key: 'albumartist', template: '%artist%' },
      { id: 'c', op: 'replace', key: 'title', pattern: '\\s+$', replacement: '' },
      { id: 'd', op: 'number', key: 'tracknumber', start: 3, pad: 2 },
      { id: 'e', op: 'delete', key: 'comment' },
    ]
    expect(opsToRequest(ops)).toEqual([
      { op: 'set', key: 'TITLE', value: 'x' },
      { op: 'ref', key: 'ALBUMARTIST', template: '%artist%' },
      { op: 'replace', key: 'TITLE', pattern: '\\s+$', replacement: '' },
      { op: 'number', key: 'TRACKNUMBER', start: 3, pad: 2 },
      { op: 'delete', key: 'COMMENT' },
    ])
  })

  it('validateOps reports the first problem or null', () => {
    expect(validateOps([])).toMatch(/操作/)
    expect(validateOps([{ id: 'a', op: 'set', key: ' ', value: 'x' }])).toMatch(/キー/)
    expect(validateOps([{ id: 'a', op: 'set', key: 'A=B', value: 'x' }])).toMatch(/キー/)
    expect(validateOps([{ id: 'a', op: 'replace', key: 'TITLE', pattern: '', replacement: '' }])).toMatch(
      /パターン/,
    )
    expect(validateOps([{ id: 'a', op: 'number', key: 'TRACKNUMBER', start: -1, pad: 0 }])).toMatch(/開始/)
    expect(validateOps([{ id: 'a', op: 'number', key: 'TRACKNUMBER', start: 1, pad: 7 }])).toMatch(/桁/)
    expect(validateOps([{ id: 'a', op: 'number', key: 'TRACKNUMBER', start: 1, pad: 1.5 }])).toMatch(/桁/)
    expect(validateOps([{ id: 'a', op: 'number', key: 'TRACKNUMBER', start: 1, pad: 6 }])).toBeNull()
    expect(validateOps([{ id: 'a', op: 'set', key: 'TITLE', value: 'x' }])).toBeNull()
    expect(validateOps([{ id: 'a', op: 'set', key: 'TITLE', value: '' }])).toBeNull()
  })

  it('moveOp reorders within bounds', () => {
    const ops: TagOp[] = [
      { id: 'a', op: 'delete', key: 'A' },
      { id: 'b', op: 'delete', key: 'B' },
      { id: 'c', op: 'delete', key: 'C' },
    ]
    expect(moveOp(ops, 'c', -1).map((o) => o.id)).toEqual(['a', 'c', 'b'])
    expect(moveOp(ops, 'a', -1).map((o) => o.id)).toEqual(['a', 'b', 'c'])
    expect(moveOp(ops, 'a', 1).map((o) => o.id)).toEqual(['b', 'a', 'c'])
    expect(moveOp(ops, 'zzz', 1)).toBe(ops)
  })
})
