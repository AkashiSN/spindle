import { describe, expect, it } from 'vitest'
import type { SyncProgress } from '../api/types'
import { agoText, mergeListProgress, phaseLabel, progressRatio, progressSummary, STALE_SECS } from './syncProgress'

const MB = 1024 * 1024
function p(over: Partial<SyncProgress> = {}): SyncProgress {
  return {
    plan_id: 1,
    done: 12,
    total: 92,
    bytes_done: 85.3 * MB,
    bytes_total: 634.8 * MB,
    rate: 12.4 * MB,
    phase: 'fetch',
    current: null,
    updated_at: 0,
    age: 0,
    ...over,
  }
}

describe('progressRatio', () => {
  it('bytes_total があればバイトで割る', () => {
    expect(progressRatio(p({ bytes_done: 50, bytes_total: 200 }))).toBe(0.25)
  })
  it('bytes_total が 0 なら件数で割る', () => {
    expect(progressRatio(p({ bytes_total: 0, bytes_done: 0, done: 1, total: 4 }))).toBe(0.25)
  })
  it('どちらも 0 なら 0、範囲は 0..1', () => {
    expect(progressRatio(p({ bytes_total: 0, total: 0, done: 0 }))).toBe(0)
    expect(progressRatio(p({ bytes_done: 300, bytes_total: 200 }))).toBe(1)
  })
})

describe('progressSummary', () => {
  it('新しい進捗は速度と残りを出す', () => {
    expect(progressSummary(p(), 0)).toBe('12 / 92 件 · 85.3 MB / 634.8 MB · 12.4 MB/s · 残り 0:44')
  })
  it('30 秒ちょうどはまだ新しい', () => {
    expect(progressSummary(p(), STALE_SECS)).toContain('MB/s')
  })
  it('31 秒は応答なし', () => {
    expect(progressSummary(p(), 31)).toBe('12 / 92 件 · 85.3 MB / 634.8 MB · 応答なし（最後の進捗: 31 秒前）')
  })
  it('bytes_total が 0 なら件数だけ', () => {
    expect(progressSummary(p({ bytes_total: 0, bytes_done: 0 }), 0)).toBe('12 / 92 件')
  })
  it('rate が 0 なら速度と残りを出さない', () => {
    expect(progressSummary(p({ rate: 0 }), 0)).toBe('12 / 92 件 · 85.3 MB / 634.8 MB')
  })
})

describe('agoText', () => {
  it('秒・分・時間', () => {
    expect(agoText(45)).toBe('45 秒前')
    expect(agoText(60)).toBe('1 分前')
    expect(agoText(125)).toBe('2 分前')
    expect(agoText(3600)).toBe('1 時間前')
  })
})

describe('phaseLabel', () => {
  it('段階の名前', () => {
    expect(phaseLabel('delete')).toBe('削除中')
    expect(phaseLabel('move')).toBe('移動中')
    expect(phaseLabel('fetch')).toBe('受信中')
    expect(phaseLabel('add')).toBe('追加中')
    expect(phaseLabel('playlist')).toBe('プレイリスト')
  })
})

describe('mergeListProgress', () => {
  const resp = [{ id: 1, sync_progress: p({ age: 2 }) }]
  it('要求より新しいイベントがあれば現在の値（null 含む）を残す', () => {
    const ev = new Map([[1, 1500]])
    expect(mergeListProgress(resp, [{ id: 1, sync_progress: null }], ev, 1000, 2000)[0].sync_progress).toBeNull()
    const newer = p({ done: 50 })
    expect(mergeListProgress(resp, [{ id: 1, sync_progress: newer }], ev, 1000, 2000)[0].sync_progress).toBe(newer)
  })
  it('要求より古いイベントは応答に負ける（receivedAt を付ける）', () => {
    const ev = new Map([[1, 900]])
    const out = mergeListProgress(resp, [{ id: 1, sync_progress: null }], ev, 1000, 5000)[0].sync_progress
    expect(out?.done).toBe(12)
    expect((out as { receivedAt?: number }).receivedAt).toBe(3000)
  })
  it('イベントが無い端末は応答のまま', () => {
    expect(mergeListProgress(resp, null, new Map(), 1000, 5000)[0].sync_progress?.done).toBe(12)
  })
})
