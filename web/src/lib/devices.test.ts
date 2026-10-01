import { describe, expect, it } from 'vitest'
import { ApiError } from '../api/client'
import type { Device, DeviceCounts, DeviceDiff, DiffItem } from '../api/types'
import { agentPlanText, pairCommand, connectionNote, nextPollDelay, UNREGISTERED_POLL_MS, jobStateChanged, offerForceAbandon, queuedSyncText, syncButton, volumeLabel, volumeUsable, deviceMessage, describeEvaluation, diffFor, OP_LABELS, withDevice, sortDiffItems, syncSummary, totalBadge, unsyncedCount } from './devices'

const counts = (p: Partial<DeviceCounts> = {}): DeviceCounts => ({
  add: 0, update: 0, move: 0, delete: 0, waiting: 0, error: 0, synced: 0, ...p,
})
const device = (id: number, c: Partial<DeviceCounts>): Device => ({
  id, name: `d${id}`, transport: 'agent', variant: 'aac', selection: 'all', generation: 1,
  connected: null, counts: counts(c), last_synced_at: null, playlist_ids: [], open_plan: false,
  adb_state: null, adb_volume: null, adb_root: null, plan_open: false, sync_job: null,
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

const base: Device = {
  id: 1, name: 'Xperia', transport: 'adb', variant: 'opus', selection: 'all', generation: 1,
  connected: true, counts: counts({ add: 1 }), last_synced_at: null, playlist_ids: [], open_plan: false,
  adb_state: 'device', adb_volume: 'E1C6-6113', adb_root: 'Music/spindle', plan_open: false, sync_job: null,
}

describe('connectionNote', () => {
  it('Android の状態を説明する', () => {
    expect(connectionNote(base)).toBeNull()
    expect(connectionNote({ ...base, connected: false, adb_state: null })).toBe('未接続（USB でつなぐと差分を取り直します）')
    expect(connectionNote({ ...base, connected: false, adb_state: 'unauthorized' })).toBe('端末で USB デバッグを許可してください')
    expect(connectionNote({ ...base, connected: false, adb_state: 'offline' })).toBe('接続中（端末の応答待ち）')
    expect(connectionNote({ ...base, transport: 'agent', connected: null })).toBeNull()
  })
})

describe('volumeLabel / volumeUsable', () => {
  it('内部と SD を分け、空でない保存先は選べない', () => {
    expect(volumeLabel({ volume: 'emulated', path: '/storage/emulated/0/Music/spindle', free: 1, state: 'missing' })).toBe('内部共有ストレージ')
    expect(volumeLabel({ volume: 'E1C6-6113', path: '/storage/E1C6-6113/Music/spindle', free: 1, state: 'empty' })).toBe('SD カード（E1C6-6113）')
    expect(volumeUsable({ volume: 'emulated', path: '', free: 1, state: 'nonempty' })).toBe(false)
    expect(volumeUsable({ volume: 'emulated', path: '', free: 1, state: 'empty' })).toBe(true)
  })
})

describe('syncButton', () => {
  const diff = { items: [{ op: 'add' }], playlists: [] } as unknown as DeviceDiff
  it('押せる条件', () => {
    expect(syncButton(base, diff)).toEqual({ enabled: true, title: null })
    expect(syncButton({ ...base, connected: false }, diff).enabled).toBe(true)
    expect(syncButton({ ...base, plan_open: true }, diff)).toEqual({ enabled: false, title: '前回の同期が途中です。続きを実行するか破棄してください' })
    expect(syncButton({ ...base, sync_job: { id: 3, state: 'running' } }, diff)).toEqual({ enabled: false, title: '同期中' })
    expect(syncButton(base, { items: [], playlists: [] } as unknown as DeviceDiff)).toEqual({ enabled: false, title: '差分がありません' })
  })
  it('待ちの同期は接続の有無で説明を変える', () => {
    const queued: Device = { ...base, sync_job: { id: 3, state: 'queued' } }
    expect(syncButton(queued, diff)).toEqual({ enabled: false, title: '同期の待ち' })
    expect(syncButton({ ...queued, connected: false }, diff)).toEqual({ enabled: false, title: '端末の接続を待っています' })
    expect(queuedSyncText(queued)).toBe('同期の待ち')
    expect(queuedSyncText({ ...queued, connected: false })).toBe('端末の接続を待っています')
  })
})

describe('offerForceAbandon', () => {
  const open: Device = { ...base, plan_open: true, open_plan: true }
  it('未接続か、破棄が not_connected で断られたときだけ強制破棄を出す', () => {
    expect(offerForceAbandon(open, null)).toBe(false)
    expect(offerForceAbandon(open, 'not_connected')).toBe(true)
    expect(offerForceAbandon({ ...open, connected: false }, null)).toBe(true)
    expect(offerForceAbandon({ ...open, connected: false, sync_job: { id: 3, state: 'queued' } }, null)).toBe(true)
  })
  it('計画が無い・同期が実行中・Android でないなら出さない', () => {
    expect(offerForceAbandon({ ...open, plan_open: false, connected: false }, null)).toBe(false)
    expect(offerForceAbandon({ ...open, connected: false, sync_job: { id: 3, state: 'running' } }, null)).toBe(false)
    expect(offerForceAbandon({ ...open, transport: 'agent', connected: null }, 'not_connected')).toBe(false)
  })
})

describe('jobStateChanged', () => {
  it('ジョブの状態が変わったときだけ true（進捗だけのイベントは false）', () => {
    const seen = new Map()
    expect(jobStateChanged(seen, { id: 1, state: 'queued' })).toBe(true)
    expect(jobStateChanged(seen, { id: 1, state: 'queued' })).toBe(false)
    expect(jobStateChanged(seen, { id: 1, state: 'running' })).toBe(true)
    expect(jobStateChanged(seen, { id: 1, state: 'running' })).toBe(false)
    expect(jobStateChanged(seen, { id: 1, state: 'cancelled' })).toBe(true)
    expect(seen.size).toBe(0)
  })
  it('終わりの状態は毎回 true で、覚えておかない（再試行で queued に戻っても拾う）', () => {
    const seen = new Map()
    expect(jobStateChanged(seen, { id: 2, state: 'done' })).toBe(true)
    expect(jobStateChanged(seen, { id: 2, state: 'failed' })).toBe(true)
    expect(jobStateChanged(seen, { id: 2, state: 'queued' })).toBe(true)
    expect(seen.get(2)).toBe('queued')
  })
})

describe('deviceMessage（Android）', () => {
  it.each([
    ['not_connected', '端末がつながっていません'],
    ['not_empty', '保存先が空ではありません'],
    ['serial_registered', 'この端末は登録済みです'],
    ['plan_changed', '差分が変わりました。確認し直してから同期してください'],
    ['open_plan_exists', '前回の同期が途中です。続きを実行するか破棄してください'],
    ['pending_reevaluation', 'スマートプレイリストの評価待ちです。評価が終わってから同期してください'],
    ['busy', '端末を別の処理が使っています。しばらくしてからやり直してください'],
    ['no_open_plan', '途中の計画はありません'],
    ['plan_unreadable', '途中の計画を読めません。破棄してください'],
    ['adb_disabled', 'Android の同期が無効です（設定の [devices].adb_server）'],
  ])('%s', (code, text) => {
    expect(deviceMessage(new ApiError(409, code))).toContain(text)
  })
})

describe('nextPollDelay', () => {
  it('取れたとき・一時的な失敗は間を置いて取り直す', () => {
    expect(nextPollDelay(null)).toBe(UNREGISTERED_POLL_MS)
    expect(nextPollDelay(new ApiError(500, 'internal'))).toBe(UNREGISTERED_POLL_MS)
    expect(nextPollDelay(new TypeError('Failed to fetch'))).toBe(UNREGISTERED_POLL_MS)
  })
  it('ADB 同期が無効なら取るのをやめる（何度取っても変わらない）', () => {
    expect(nextPollDelay(new ApiError(503, 'adb_disabled'))).toBeNull()
  })
  it('中止（フォームを閉じた）ならやめる', () => {
    expect(nextPollDelay(new DOMException('aborted', 'AbortError'))).toBeNull()
  })
})

describe('agentPlanText', () => {
  it('iPhone の途中の計画だけ案内する', () => {
    expect(agentPlanText({ ...device(1, {}), transport: 'agent', plan_open: true })).toMatch('Mac で同期が途中')
    expect(agentPlanText({ ...device(1, {}), transport: 'agent', plan_open: false })).toBeNull()
    expect(agentPlanText({ ...device(1, {}), transport: 'adb', plan_open: true })).toBeNull()
  })
})

describe('pairCommand', () => {
  it('この画面の origin とコードを並べる', () => {
    expect(pairCommand('https://musics.example', 'abc.def')).toBe('spindle-agent pair https://musics.example abc.def')
  })
})
