import { describe, expect, it } from 'vitest'
import type { PreviewResponse } from '../api/types'
import {
  cellDiff,
  formatValues,
  indexPreview,
  inlineEditOps,
  previewKey,
  visiblePendingPrompt,
  type PreviewState,
} from './preview'
import { NO_SELECTION, type Selection } from './selection'

const res: PreviewResponse = {
  selection_token: 'tok',
  count: 3,
  changed: 1,
  unchanged: 1,
  pending_excluded: 1,
  items: [{ id: 5, changes: { TITLE: { old: ['a'], new: ['b'] }, COMMENT: { old: null, new: ['c'] } } }],
}

describe('preview', () => {
  it('indexPreview maps items by id and keeps counts and key', () => {
    const st = indexPreview(res, 'k1')
    expect(st.token).toBe('tok')
    expect(st.key).toBe('k1')
    expect(st.counts).toEqual({ count: 3, changed: 1, unchanged: 1, pending_excluded: 1 })
    expect(st.changesById.get(5)?.TITLE).toEqual({ old: ['a'], new: ['b'] })
    expect(st.changesById.has(6)).toBe(false)
  })

  it('cellDiff resolves the tag key of a column', () => {
    const st = indexPreview(res, 'k')
    expect(cellDiff(st, 5, 'title')).toEqual({ old: ['a'], new: ['b'] })
    expect(cellDiff(st, 5, 'artist')).toBeNull()
    expect(cellDiff(st, 6, 'title')).toBeNull()
    expect(cellDiff(st, 5, 'badges')).toBeNull()
    expect(cellDiff(null, 5, 'title')).toBeNull()
  })

  it('formatValues joins multi values and shows empty', () => {
    expect(formatValues(['a', 'b'])).toBe('a, b')
    expect(formatValues(null)).toBe('')
    expect(formatValues([])).toBe('')
  })

  it('previewKey changes with selection, ops and sort', () => {
    const sel: Selection = { kind: 'ids', ids: new Set([2, 1]), anchor: 1 }
    const ops = [{ op: 'set', key: 'TITLE', value: 'x' }]
    const a = previewKey(sel, ops, 'title')
    expect(previewKey({ kind: 'ids', ids: new Set([1, 2]), anchor: 2 }, ops, 'title')).toBe(a)
    expect(previewKey(sel, ops, '-title')).not.toBe(a)
    expect(previewKey(sel, [{ op: 'set', key: 'TITLE', value: 'y' }], 'title')).not.toBe(a)
    expect(previewKey(NO_SELECTION, ops, 'title')).not.toBe(a)
    const f: Selection = { kind: 'filter', filter: '{}', excludeIds: new Set([3]), anchor: null }
    expect(previewKey(f, ops, 'title')).not.toBe(previewKey({ ...f, excludeIds: new Set() }, ops, 'title'))
  })

  it('inlineEditOps maps editable columns to a set op and rejects others', () => {
    expect(inlineEditOps('title', ' New ')).toEqual([{ op: 'set', key: 'TITLE', value: 'New' }])
    expect(inlineEditOps('artist', 'A')).toEqual([{ op: 'set', key: 'ARTIST', value: 'A' }])
    expect(inlineEditOps('album', 'A')).toEqual([{ op: 'set', key: 'ALBUM', value: 'A' }])
    expect(inlineEditOps('albumartist', 'A')).toEqual([{ op: 'set', key: 'ALBUMARTIST', value: 'A' }])
    expect(inlineEditOps('date', '2020')).toEqual([{ op: 'set', key: 'DATE', value: '2020' }])
    expect(inlineEditOps('no', '3')).toEqual([{ op: 'set', key: 'TRACKNUMBER', value: '3' }])
    expect(inlineEditOps('no', '2-03')).toEqual([
      { op: 'set', key: 'DISCNUMBER', value: '2' },
      { op: 'set', key: 'TRACKNUMBER', value: '3' },
    ])
    expect(inlineEditOps('no', 'x')).toBeNull()
    expect(inlineEditOps('codec', 'x')).toBeNull()
    expect(inlineEditOps('title', '')).toEqual([{ op: 'set', key: 'TITLE', value: '' }])
  })

  it('visiblePendingPrompt hides the prompt once the preview key changed', () => {
    const prompt = { key: 'k1', count: 2, trackIds: [1, 2] }
    expect(visiblePendingPrompt(prompt, 'k1')).toBe(prompt)
    expect(visiblePendingPrompt(prompt, 'k2')).toBeNull()
    expect(visiblePendingPrompt(null, 'k1')).toBeNull()
    // 選択・操作・ソートのどれを変えても key が変わる（= 確認が消える）
    const sel: Selection = { kind: 'ids', ids: new Set([1]), anchor: 1 }
    const ops = [{ op: 'set', key: 'TITLE', value: 'x' }]
    const k = previewKey(sel, ops, 'title')
    expect(visiblePendingPrompt({ key: k, count: 1, trackIds: [1] }, previewKey({ kind: 'ids', ids: new Set([2]), anchor: 2 }, ops, 'title'))).toBeNull()
    expect(visiblePendingPrompt({ key: k, count: 1, trackIds: [1] }, previewKey(sel, [{ op: 'delete', key: 'X' }], 'title'))).toBeNull()
    expect(visiblePendingPrompt({ key: k, count: 1, trackIds: [1] }, previewKey(sel, ops, '-title'))).toBeNull()
  })

  it('state type carries the token for apply', () => {
    const st: PreviewState = indexPreview(res, 'k')
    expect(typeof st.token).toBe('string')
  })
})
