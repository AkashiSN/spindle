import { describe, expect, it } from 'vitest'
import type { Device, PlaylistSyncState } from '../api/types'
import { applyProgressEvent } from './syncProgress'
import { chipsFor, mergePending, sameIdSet, toggleDraft, togglePlaylist, withPlaylistIds } from './devicePicker'

const d = (id: number, name: string, playlist_ids: number[], playlist_states: PlaylistSyncState[] = []): Device => ({
  id, name, transport: 'agent', variant: 'aac', selection: 'playlists', generation: 1, connected: null,
  counts: { add: 0, update: 0, move: 0, delete: 0, waiting: 0, error: 0, synced: 0 },
  last_synced_at: null, playlist_ids, playlist_states, open_plan: false, adb_state: null, adb_volume: null, adb_root: null, plan_open: false, sync_job: null,
})
const st = (
  playlist_id: number,
  state: PlaylistSyncState['state'],
  more: Partial<PlaylistSyncState> = {},
): PlaylistSyncState => ({ playlist_id, state, pending: 0, waiting: 0, errors: 0, reason: null, ...more })

describe('選曲の近道', () => {
  it('チップはその印が付いた端末だけ、名前順', () => {
    const got = chipsFor(5, [d(1, 'Xperia', [5]), d(2, 'iPhone', [5, 6]), d(3, 'iPad', [6])])
    expect(got.map((x) => x.device.name)).toEqual(['iPhone', 'Xperia'])
  })
  it('チップは同期の状況と、端末名つきの説明を持つ', () => {
    const states = [
      st(1, 'synced'),
      st(2, 'pending', { pending: 3 }),
      st(3, 'pending'),
      st(4, 'error', { errors: 2 }),
      st(5, 'error', { reason: 'パス衝突' }),
      st(6, 'removing'),
      st(8, 'pending', { waiting: 2 }),
      st(9, 'pending', { pending: 1, waiting: 2 }),
    ]
    const dev = d(1, 'iPhone', [1, 2, 3, 4, 5, 7, 8, 9], states)
    const one = (pid: number) => chipsFor(pid, [dev]).map((c) => [c.status, c.title])
    expect(one(1)).toEqual([['synced', 'iPhone: 同期済み']])
    expect(one(2)).toEqual([['pending', 'iPhone: 同期待ち（3 曲）']])
    expect(one(3)).toEqual([['pending', 'iPhone: 同期待ち（プレイリストの更新）']])
    expect(one(4)).toEqual([['error', 'iPhone: 2 曲を送れない']])
    expect(one(5)).toEqual([['error', 'iPhone: 送れない（パス衝突）']])
    // 選曲から外したが端末に残っている
    expect(one(6)).toEqual([['removing', 'iPhone: 次の同期で端末から消える']])
    // 付けたばかりで状況がまだ無い
    expect(one(7)).toEqual([['pending', 'iPhone: 同期待ち']])
    expect(one(8)).toEqual([['pending', 'iPhone: 準備待ち（2 曲）']])
    expect(one(9)).toEqual([['pending', 'iPhone: 同期待ち（1 曲）・準備待ち（2 曲）']])
  })
  const states = [
    st(1, 'synced'),
    st(2, 'pending', { pending: 1 }),
    st(3, 'pending'),
    st(4, 'error', { errors: 1 }),
    st(5, 'pending', { waiting: 2 }),
  ]
  const statuses = (dev: Device) => [1, 2, 3, 4, 5].map((pid) => chipsFor(pid, [dev])[0].status)
  it('Android の同期ジョブの実行中は、送るものがある同期待ちだけを同期中にする', () => {
    const dev = { ...d(1, 'Xperia', [1, 2, 3, 4, 5], states), sync_job: { id: 9, state: 'running' as const } }
    // 送れないもの・準備待ちだけのものは、実行中の同期では片付かないのでそのまま
    expect(statuses(dev)).toEqual(['synced', 'syncing', 'syncing', 'error', 'pending'])
    expect(chipsFor(2, [dev])[0].title).toBe('Xperia: 同期中')
    // 待ち（queued）は同期中にしない
    expect(statuses({ ...dev, sync_job: { id: 9, state: 'queued' } })).toEqual(['synced', 'pending', 'pending', 'error', 'pending'])
  })
  it('iPhone は Mac の同期の進捗（SSE の device_progress）が来ている間を同期中にする', () => {
    const dev = d(1, 'iPhone', [1, 2, 3, 4, 5], states)
    expect(statuses(dev)).toEqual(['synced', 'pending', 'pending', 'error', 'pending'])
    const progress = {
      plan_id: 3, done: 1, total: 10, bytes_done: 0, bytes_total: 0, rate: 0,
      phase: 'add' as const, current: null, updated_at: 0, age: 0,
    }
    const [running] = applyProgressEvent([dev], { device_id: 1, progress }, 0)
    expect(statuses(running)).toEqual(['synced', 'syncing', 'syncing', 'error', 'pending'])
    // 計画が閉じたら戻る
    const [closed] = applyProgressEvent([running], { device_id: 1, progress: null }, 0)
    expect(statuses(closed)).toEqual(['synced', 'pending', 'pending', 'error', 'pending'])
  })
  it('外したばかり（状況がまだ synced のまま）のプレイリストには出さない', () => {
    expect(chipsFor(1, [d(1, 'iPhone', [], [st(1, 'synced')])])).toEqual([])
  })
  it('印を付け外しする', () => {
    expect(togglePlaylist(d(1, 'a', [6, 2]), 5)).toEqual([2, 5, 6])
    expect(togglePlaylist(d(1, 'a', [5, 6]), 5)).toEqual([6])
  })
  it('楽観更新: 1 台の playlist_ids だけ差し替え、続けての切り替えが前の印を落とさない', () => {
    const items = [d(1, 'a', []), d(2, 'b', [9])]
    const after = withPlaylistIds(items, 1, togglePlaylist(items[0], 5))
    const next = withPlaylistIds(after, 1, togglePlaylist(after![0], 6))
    expect(next!.map((x) => x.playlist_ids)).toEqual([[5, 6], [9]])
    expect(withPlaylistIds(null, 1, [1])).toBeNull()
  })
  it('取り直した一覧に、送信中の端末の望む印を重ねる', () => {
    const fetched = [d(1, 'a', [5]), d(2, 'b', [9])]
    const merged = mergePending(fetched, new Map([[1, [5, 6]]]))
    expect(merged.map((x) => x.playlist_ids)).toEqual([[5, 6], [9]])
    expect(mergePending(fetched, new Map()).map((x) => x.playlist_ids)).toEqual([[5], [9]])
  })
})

describe('選曲タブの下書き', () => {
  it('未編集なら保存済みから始め、保存済みと同じに戻れば未編集（null）に戻る', () => {
    expect(toggleDraft(null, [5], 6, true)).toEqual([5, 6])
    expect(toggleDraft([5, 6], [5], 6, false)).toBeNull()
    expect(toggleDraft(null, [5], 5, true)).toBeNull()
  })
  it('編集中はサイドバーの変更（保存済みの値）ではなく下書きを使う', () => {
    const draft = toggleDraft(null, [5], 7, true)
    expect(draft).toEqual([5, 7])
    // サイドバーで 6 を付けた後（保存済みが [5, 6]）も下書きは変わらない
    expect(toggleDraft(draft, [5, 6], 8, true)).toEqual([5, 7, 8])
  })
  it('順序を問わず同じ集合か', () => {
    expect(sameIdSet([1, 2], [2, 1])).toBe(true)
    expect(sameIdSet([1, 2], [1])).toBe(false)
  })
})
