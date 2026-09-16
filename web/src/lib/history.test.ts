import { describe, expect, it } from 'vitest'
import type { HistoryItem } from '../api/types'
import {
  batchLabel,
  canCancel,
  canRevert,
  formatDateTime,
  revertedNote,
  stateLabel,
} from './history'

function item(over: Partial<HistoryItem> = {}): HistoryItem {
  return {
    id: 42,
    created_at: 1_758_000_000,
    description: 'Official を除去',
    kind: 'tags',
    state: 'applied',
    affected: 1204,
    applied: 1204,
    conflict: 0,
    failed: 0,
    reverts_batch_id: null,
    reverted_by: null,
    finished_at: 1_758_000_100,
    reverted_at: null,
    ...over,
  }
}

describe('batchLabel', () => {
  it('uses the description when present', () => {
    expect(batchLabel(item())).toBe('Official を除去')
  })
  it('names the reverted batch when the description is empty', () => {
    expect(batchLabel(item({ description: null, reverts_batch_id: 38 }))).toBe('(#38 の巻き戻し)')
  })
  it('falls back to the kind', () => {
    expect(batchLabel(item({ description: null, kind: 'rename' }))).toBe('(rename)')
    expect(batchLabel(item({ description: '', kind: null }))).toBe('(不明)')
  })
})

describe('stateLabel', () => {
  it('shows progress while applying', () => {
    expect(
      stateLabel(item({ state: 'applying', affected: 980, applied: 600, conflict: 30, failed: 10 })),
    ).toBe('applying 640/980')
    expect(stateLabel(item({ state: 'prepared', affected: 5, applied: 0 }))).toBe('prepared 0/5')
  })
  it('shows conflict and failed counts for partial batches', () => {
    expect(stateLabel(item({ state: 'partial', conflict: 3 }))).toBe('partial (conflict 3)')
    expect(stateLabel(item({ state: 'partial', conflict: 3, failed: 2 }))).toBe(
      'partial (conflict 3, failed 2)',
    )
  })
  it('is plain for applied, failed and cancelled', () => {
    expect(stateLabel(item())).toBe('applied')
    expect(stateLabel(item({ state: 'cancelled', applied: 0, failed: 5 }))).toBe('cancelled')
  })
})

describe('canRevert / canCancel', () => {
  it('reverts only terminal batches that are not fully reverted', () => {
    expect(canRevert(item())).toBe(true)
    expect(canRevert(item({ state: 'partial' }))).toBe(true)
    expect(canRevert(item({ state: 'applying' }))).toBe(false)
    expect(canRevert(item({ state: 'prepared' }))).toBe(false)
    expect(canRevert(item({ reverted_at: 1, reverted_by: 39 }))).toBe(false)
  })
  it('does not offer revert when nothing was applied', () => {
    expect(canRevert(item({ state: 'failed', applied: 0, conflict: 3 }))).toBe(false)
    expect(canRevert(item({ state: 'cancelled', applied: 1, failed: 4 }))).toBe(true)
  })
  it('cancels only open batches', () => {
    expect(canCancel(item({ state: 'prepared' }))).toBe(true)
    expect(canCancel(item({ state: 'applying' }))).toBe(true)
    expect(canCancel(item())).toBe(false)
  })
})

describe('revertedNote', () => {
  it('names the reverse batch', () => {
    expect(revertedNote(item({ reverted_at: 1, reverted_by: 39 }))).toBe('#39 で戻し済み')
    expect(revertedNote(item({ reverted_at: 1, reverted_by: null }))).toBe('戻し済み')
    expect(revertedNote(item())).toBeNull()
  })
})

describe('formatDateTime', () => {
  it('formats as MM-DD HH:MM in local time', () => {
    const d = new Date(2026, 8, 16, 14, 3)
    expect(formatDateTime(Math.floor(d.getTime() / 1000))).toBe('09-16 14:03')
  })
})
