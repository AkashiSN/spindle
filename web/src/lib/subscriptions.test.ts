import { describe, expect, it } from 'vitest'
import type { Job, Subscription, SyncResult } from '../api/types'
import {
  activeSyncJob,
  blockedReasonLabel,
  listIdFromUrl,
  subscriptionStatusLabel,
  syncDetailLines,
  syncSummary,
} from './subscriptions'

function job(over: Partial<Job>): Job {
  return {
    id: 1,
    type: 'playlist_sync',
    state: 'queued',
    progress: null,
    done: null,
    total: null,
    attempts: 0,
    max_attempts: 5,
    last_error: null,
    run_after: null,
    edit_batch_id: null,
    created_at: 1,
    started_at: null,
    finished_at: null,
    subject: 'subscription #3',
    ...over,
  }
}

function sub(over: Partial<Subscription>): Subscription {
  return {
    id: 3,
    list_id: 'PL1',
    url: 'https://www.youtube.com/playlist?list=PL1',
    album_id: null,
    albumartist: 'A',
    album: 'B',
    category: null,
    align: true,
    enabled: true,
    max_enqueue: 50,
    created_at: 1,
    updated_at: 1,
    last_attempted_at: null,
    last_synced_at: null,
    sync_requested_at: null,
    last_result: null,
    ...over,
  }
}

function result(over: Partial<SyncResult>): SyncResult {
  return {
    state: 'done',
    synced_at: 10,
    entries: 4,
    in_library: 3,
    in_inbox: 0,
    elsewhere: [],
    enqueued: [],
    running: [],
    deferred: 0,
    unavailable: [],
    ...over,
  }
}

describe('listIdFromUrl', () => {
  it('YouTube の list= だけを受ける', () => {
    expect(listIdFromUrl(' https://www.youtube.com/playlist?list=PLabc-_1 ')).toBe('PLabc-_1')
    expect(listIdFromUrl('https://music.youtube.com/watch?v=x&list=PL9')).toBe('PL9')
    expect(listIdFromUrl('https://www.youtube.com/watch?v=x')).toBeNull()
    expect(listIdFromUrl('https://example.com/?list=PL1')).toBeNull()
    expect(listIdFromUrl('https://notyoutube.com/?list=PL1')).toBeNull()
    expect(listIdFromUrl('javascript:alert(1)')).toBeNull()
    expect(listIdFromUrl('https://www.youtube.com/playlist?list=bad%20id')).toBeNull()
    expect(listIdFromUrl('')).toBeNull()
  })
})

describe('activeSyncJob', () => {
  it('購読の queued / running の同期ジョブを引く', () => {
    const jobs = [
      job({ id: 1, state: 'done' }),
      job({ id: 2, state: 'running' }),
      job({ id: 3, subject: 'subscription #4' }),
      job({ id: 4, type: 'ytdl', subject: 'subscription #3' }),
    ]
    expect(activeSyncJob(jobs, 3)?.id).toBe(2)
    expect(activeSyncJob(jobs, 4)?.id).toBe(3)
    expect(activeSyncJob(jobs, 5)).toBeUndefined()
  })
})

describe('subscriptionStatusLabel / syncSummary', () => {
  it('走行中・未同期・失敗・要約', () => {
    expect(subscriptionStatusLabel(sub({}), job({ state: 'running' }))).toBe('同期中')
    expect(subscriptionStatusLabel(sub({}), job({}))).toBe('同期待ち')
    expect(subscriptionStatusLabel(sub({}), job({ attempts: 1, last_error: '取りこぼした' }))).toBe(
      '再試行待ち（取りこぼした）',
    )
    expect(subscriptionStatusLabel(sub({}), undefined)).toBe('未同期')
    expect(subscriptionStatusLabel(sub({ sync_requested_at: 5 }), undefined)).toBe('同期待ち')
    expect(subscriptionStatusLabel(sub({ last_result: result({ state: 'failed', error: 'x' }) }), undefined)).toBe(
      '失敗: x',
    )
    expect(subscriptionStatusLabel(sub({ last_result: result({ state: 'cancelled' }) }), undefined)).toBe('取り消し')
    expect(subscriptionStatusLabel(sub({ last_result: result({}) }), undefined)).toBe('4 件中 Library に 3')
  })
  it('要約はサーバの note と同じ順', () => {
    expect(
      syncSummary(
        result({
          enqueued: [3, 6],
          in_inbox: 1,
          running: [7],
          deferred: 2,
          unavailable: [{ position: 2, id: 'p', kind: 'unknown' }],
          elsewhere: [{ position: 4, id: 'e', track_id: 9, rel_path: 'x' }],
          align: { moved: 1, unchanged: 2, blocked: [], outsiders: 0, unnumbered: 0, renamed: 1 },
        }),
      ),
    ).toBe(
      '4 件中 Library に 3、2 件を投入、1 件は Inbox で取り込み中、1 件は別の投入が走行中、2 件は次回、1 件は取れない、1 件は別の album、番号を 1 件揃え 1 件を改名',
    )
    expect(
      syncSummary(
        result({
          align: {
            moved: 0,
            unchanged: 3,
            blocked: [{ track_id: 1, position: 2, current_no: 4, reason: { kind: 'number_taken', by_track_id: 20 } }],
            outsiders: 0,
            unnumbered: 1,
            renamed: 0,
          },
        }),
      ),
    ).toBe('4 件中 Library に 3、1 件は揃えられない')
  })
})

describe('syncDetailLines', () => {
  it('取れない・別 album・揃えられない・バッチの行', () => {
    const lines = syncDetailLines(
      result({
        unavailable: [
          { position: 2, id: 'p', kind: 'private' },
          { position: 5, id: 'q', kind: 'unknown' },
        ],
        elsewhere: [{ position: 4, id: 'e', track_id: 9, rel_path: 'Other/01 e.opus' }],
        running: [7],
        deferred: 1,
        align: {
          moved: 1,
          unchanged: 2,
          blocked: [
            { track_id: 11, position: 2, current_no: 4, reason: { kind: 'number_taken', by_track_id: 20 } },
            { track_id: 12, position: 3, current_no: null, reason: { kind: 'duplicate_source_url' } },
            { track_id: 13, position: 6, current_no: 1, reason: { kind: 'other_disc', disc_no: 2 } },
          ],
          outsiders: 1,
          unnumbered: 2,
          tags: { batch_id: 30, applied: 1, conflict: 0, failed: 0 },
          renamed: 1,
          rename: { batch_id: 31, applied: 0, conflict: 1, failed: 0 },
        },
      }),
    )
    expect(lines).toEqual([
      '取れない: #2 p（非公開）、#5 q（取れない）',
      '別の album にある: #4 → Other/01 e.opus',
      '別の投入が走行中: #7',
      '上限で次回に持ち越し: 1 件',
      '揃えられない: #2（今 4）: 2 番は track #20 が使っている（SOURCE_URL 無しか再生リスト外）',
      '揃えられない: #3（今 無番）: 同じ SOURCE_URL の行が複数ある',
      '揃えられない: #6（今 1）: disc 2 の行',
      '番号のバッチ #30: 適用 1 / 衝突 0 / 失敗 0',
      '改名のバッチ #31: 適用 0 / 衝突 1 / 失敗 0',
      '再生リストに無い SOURCE_URL 付きの行: 1 件（触らない）',
      'SOURCE_URL の無い行: 2 件（触らない）',
    ])
    expect(syncDetailLines(result({}))).toEqual([])
    const dup = syncDetailLines(
      result({
        align: {
          moved: 0,
          unchanged: 1,
          blocked: [{ track_id: 5, position: 1, current_no: 1, reason: { kind: 'duplicate_entry', positions: [1, 3] } }],
          outsiders: 0,
          unnumbered: 0,
          renamed: 0,
          rename_conflicts: [{ track_id: 6, reason: '同名のファイルがある' }],
        },
      }),
    )
    expect(dup).toEqual([
      '揃えられない: #1（今 1）: 同じ動画が再生リストに複数回ある（#1、#3）',
      '改名できない: track #6: 同名のファイルがある',
    ])
    expect(
      syncSummary(
        result({
          align: { moved: 0, unchanged: 1, blocked: [], outsiders: 0, unnumbered: 0, renamed: 0, rename_conflicts: [{ track_id: 6, reason: 'x' }] },
        }),
      ),
    ).toBe('4 件中 Library に 3、1 件は改名できない')
    expect(blockedReasonLabel({ track_id: 1, position: 1, current_no: null, reason: { kind: 'duplicate_source_url' } })).toBe(
      '同じ SOURCE_URL の行が複数ある',
    )
  })
})
