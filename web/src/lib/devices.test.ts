import { describe, expect, it } from 'vitest'
import { ApiError } from '../api/client'
import type { Device, DeviceCounts, DeviceDiff, DiffItem } from '../api/types'
import { deviceMessage, describeEvaluation, diffFor, OP_LABELS, withDevice, sortDiffItems, syncSummary, totalBadge, unsyncedCount } from './devices'

const counts = (p: Partial<DeviceCounts> = {}): DeviceCounts => ({
  add: 0, update: 0, move: 0, delete: 0, waiting: 0, error: 0, synced: 0, ...p,
})
const device = (id: number, c: Partial<DeviceCounts>): Device => ({
  id, name: `d${id}`, transport: 'agent', variant: 'aac', selection: 'all', generation: 1,
  connected: null, counts: counts(c), last_synced_at: null, playlist_ids: [], open_plan: false,
})
const item = (op: DiffItem['op'], title: string, size = 0): DiffItem => ({
  op, track_id: title.length, title, artist: null, from: null, dest_path: null, reason: null, size, has_copy: false,
})

describe('端末の件数', () => {
  it('未反映は追加・更新・移動・削除・エラーで、待ちと反映済みは数えない', () => {
    expect(unsyncedCount(counts({ add: 1, update: 2, move: 3, delete: 4, error: 5, waiting: 100, synced: 100 }))).toBe(15)
  })
  it('ナビのバッジは全端末の合計（同じ曲が 2 端末で未反映なら 2）', () => {
    expect(totalBadge([device(1, { add: 1 }), device(2, { add: 1, error: 1 })])).toBe(3)
    expect(totalBadge([])).toBe(0)
  })
})

describe('差分表', () => {
  it('操作の名前がそろっている', () => {
    expect(OP_LABELS.update_move).toBe('更新 + 移動')
    expect(OP_LABELS.waiting).toBe('待ち')
  })
  it('エラー → 削除 → 更新 + 移動 → 移動 → 更新 → 追加 → 待ち の順、同じ操作は曲名順', () => {
    const got = sortDiffItems([item('waiting', 'w'), item('add', 'b'), item('add', 'a'), item('error', 'e'), item('delete', 'd'), item('move', 'm')])
    expect(got.map((i) => `${i.op}:${i.title}`)).toEqual(['error:e', 'delete:d', 'move:m', 'add:a', 'add:b', 'waiting:w'])
  })
  it('送る件数は待ちとエラーを除き、量は見積もりの transfer_bytes', () => {
    const d = {
      items: [item('add', 'a', 10), item('delete', 'd'), item('waiting', 'w'), item('error', 'e')],
      estimate: { transfer_bytes: 10, peak_bytes: 10, free: null },
    } as unknown as DeviceDiff
    expect(syncSummary(d)).toEqual({ count: 2, bytes: 10 })
  })
  it('評価時刻の説明（評価待ちを優先）', () => {
    expect(describeEvaluation({ playlist_id: 1, name: 'p', evaluated_at: 100, pending: true }, 200)).toContain('評価待ち')
    expect(describeEvaluation({ playlist_id: 1, name: 'p', evaluated_at: null, pending: false }, 200)).toContain('未評価')
  })
})

describe('エラーの文言', () => {
  it('open_plan と cycle を日本語にする', () => {
    expect(deviceMessage(new ApiError(409, 'open_plan', 'x'))).toContain('同期が途中')
    expect(deviceMessage(new ApiError(400, 'cycle', 'x'))).toContain('端末の状態')
  })
})

describe('選んでいる端末', () => {
  const diff = { generation: 1 } as unknown as DeviceDiff
  it('差分は選んでいる端末のものだけ出す（切り替え直後に前の端末の差分を出さない）', () => {
    expect(diffFor({ id: 1, diff }, 1)).toBe(diff)
    expect(diffFor({ id: 1, diff }, 2)).toBeNull()
    expect(diffFor({ id: 1, diff }, null)).toBeNull()
    expect(diffFor(null, 1)).toBeNull()
  })
  it('作った端末は一覧の取り直しを待たずに一覧に入る', () => {
    expect(withDevice([device(1, {})], device(2, {})).map((d) => d.id)).toEqual([1, 2])
    expect(withDevice([device(1, {}), device(2, {})], device(2, {})).map((d) => d.id)).toEqual([1, 2])
    expect(withDevice(null, device(3, {})).map((d) => d.id)).toEqual([3])
  })
})
