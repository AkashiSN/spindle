import { describe, expect, it } from 'vitest'
import type { Job, Subscription } from '../api/types'
import type { SyncResult } from '../api/types'
import { cardSummary, downloadCounts, itemLabel, resultBreakdown, sortItems, syncButton, syncedAgo, syncPhase, unavailableLine } from './subscriptions'

const job = (over: Partial<Job>): Job => ({
  id: 1,
  type: 'ytdl',
  state: 'done',
  progress: null,
  done: null,
  total: null,
  attempts: 1,
  max_attempts: 3,
  last_error: null,
  run_after: null,
  edit_batch_id: null,
  created_at: 0,
  started_at: null,
  finished_at: null,
  note: null,
  subject: null,
  payload: {},
  ...over,
})
const sync = (state: Job['state']) => job({ type: 'playlist_sync', state })

describe('syncPhase', () => {
  it('syncPhase はダウンロードが全部終わるまで running', () => {
    expect(syncPhase({ sync: null, items: [] })).toBe('idle')
    expect(syncPhase({ sync: sync('running'), items: [] })).toBe('running')
    expect(syncPhase({ sync: sync('done'), items: [job({ state: 'queued' })] })).toBe('running')
    expect(syncPhase({ sync: sync('done'), items: [job({}), job({ state: 'failed' })] })).toBe('failed')
    expect(syncPhase({ sync: sync('failed'), items: [] })).toBe('failed')
    expect(syncPhase({ sync: sync('done'), items: [job({})] })).toBe('done')
  })
  it('取り消しも失敗に数える', () => {
    expect(syncPhase({ sync: sync('cancelled'), items: [] })).toBe('failed')
    expect(syncPhase({ sync: sync('done'), items: [job({ state: 'cancelled' })] })).toBe('failed')
  })
})

describe('downloadCounts', () => {
  it('完了・失敗・全体を数える', () => {
    expect(
      downloadCounts({
        sync: sync('done'),
        items: [job({}), job({ state: 'failed' }), job({ state: 'running' }), job({ state: 'cancelled' })],
      }),
    ).toEqual({ done: 1, total: 4, failed: 2 })
  })
})

describe('cardSummary', () => {
  it('cardSummary は結果を 1 行で', () => {
    const s = { last_result: { unavailable: [{ position: 31, id: 'x', kind: 'private' }] } } as unknown as Subscription
    expect(cardSummary(s, { sync: sync('done'), items: [job({}), job({})] })).toBe('✓ 完了 · 新しく 2 曲を Inbox へ · 非公開 1')
    expect(cardSummary({ last_result: { unavailable: [] } } as unknown as Subscription, { sync: sync('done'), items: [] })).toBe(
      '✓ 完了 · 新しい曲なし',
    )
    expect(cardSummary({ last_result: null } as Subscription, { sync: sync('done'), items: [job({}), job({ state: 'failed' })] })).toBe(
      '✕ 失敗 · 1 曲ダウンロードできなかった',
    )
    expect(
      cardSummary({ last_result: null } as Subscription, {
        sync: job({ type: 'playlist_sync', state: 'running' }),
        items: [job({}), job({ state: 'running' })],
      }),
    ).toBe('ダウンロード 1 / 2 曲')
  })
  it('取れない動画は種類ごとに数える', () => {
    const s = {
      last_result: {
        unavailable: [
          { position: 1, id: 'a', kind: 'deleted' },
          { position: 2, id: 'b', kind: 'private' },
          { position: 3, id: 'c', kind: 'private' },
        ],
      },
    } as unknown as Subscription
    expect(cardSummary(s, { sync: sync('done'), items: [] })).toBe('✓ 完了 · 新しい曲なし · 非公開 2 · 削除 1')
  })
  it('同期そのものの失敗は理由を出す', () => {
    const s = { last_result: null } as Subscription
    expect(cardSummary(s, { sync: job({ type: 'playlist_sync', state: 'failed', last_error: 'yt-dlp が失敗\n詳細' }), items: [] })).toBe(
      '✕ 失敗 · yt-dlp が失敗',
    )
    expect(cardSummary(s, { sync: job({ type: 'playlist_sync', state: 'cancelled' }), items: [] })).toBe('✕ 取り消し')
  })
  it('投入前は再生リストの確認中', () => {
    const s = { last_result: null } as Subscription
    expect(cardSummary(s, { sync: sync('queued'), items: [] })).toBe('同期待ち')
    expect(cardSummary(s, { sync: sync('running'), items: [] })).toBe('再生リストを確認中')
  })
  it('ジョブが無ければ前回の結果か未同期', () => {
    expect(cardSummary({ last_result: null } as Subscription, { sync: null, items: [] })).toBe('未同期')
    const failed = { last_result: { state: 'failed', error: '取れない', unavailable: [], enqueued: [] } } as unknown as Subscription
    expect(cardSummary(failed, { sync: null, items: [] })).toBe('✕ 前回は失敗 · 取れない')
    const done = { last_result: { state: 'done', unavailable: [], enqueued: [3, 4] } } as unknown as Subscription
    expect(cardSummary(done, { sync: null, items: [] })).toBe('前回 2 曲を投入')
    const none = { last_result: { state: 'done', unavailable: [], enqueued: [] } } as unknown as Subscription
    expect(cardSummary(none, { sync: null, items: [] })).toBe('前回は新しい曲なし')
  })
})

describe('itemLabel', () => {
  it('itemLabel は完了の note を曲名に使い、無ければ位置番号', () => {
    expect(itemLabel(job({ note: 'Inbox に置いた: メロウ', payload: { position: 2 } })).trail).toBe('Inbox へ')
    expect(itemLabel(job({ state: 'running', progress: 0.62, payload: { position: 4 } }))).toEqual({ icon: '⟳', text: '#4', trail: '62%' })
  })
  it('Inbox のパスは日付・動画 id・拡張子を外して曲名にする', () => {
    expect(itemLabel(job({ note: 'Inbox に置いた: ytmusic/Artist/20260101 メロウ [abc_D-12].opus', payload: { position: 2 } }))).toEqual({
      icon: '✓',
      text: 'メロウ',
      trail: 'Inbox へ',
    })
  })
  it('形の違うファイル名はそのまま', () => {
    expect(itemLabel(job({ note: 'Inbox に置いた: x/02 メロウ.opus' })).text).toBe('02 メロウ')
  })
  it('待ち・失敗・取り込み済み', () => {
    expect(itemLabel(job({ state: 'queued', attempts: 0, payload: { position: 5 } }))).toEqual({ icon: '…', text: '#5', trail: '待ち' })
    expect(itemLabel(job({ state: 'queued', attempts: 1, last_error: 'x', payload: { position: 5 } })).trail).toBe('再試行待ち')
    expect(itemLabel(job({ state: 'failed', last_error: 'HTTP 403\nstderr', payload: { position: 6 } }))).toEqual({
      icon: '✕',
      text: '#6',
      trail: 'HTTP 403',
    })
    expect(itemLabel(job({ state: 'cancelled', payload: { position: 7 } }))).toEqual({ icon: '✕', text: '#7', trail: '取り消し' })
    expect(itemLabel(job({ note: '取り込み済み（library）: a/b.flac', payload: { position: 8 } })).trail).toBe('取り込み済み')
    expect(itemLabel(job({ state: 'running', payload: {} }))).toEqual({ icon: '⟳', text: '#?', trail: 'ダウンロード中' })
  })
})

describe('syncedAgo', () => {
  it('相対表記。1 日を超えたら日', () => {
    expect(syncedAgo(null, 1000)).toBe('未同期')
    expect(syncedAgo(1000 - 90, 1000)).toBe('1 分前')
    expect(syncedAgo(1000 - 86400 * 3, 1000)).toBe('3 日前')
  })
})

describe('詳細の行', () => {
  it('内訳と取れない動画', () => {
    const r = { entries: 12, in_library: 9, in_inbox: 1, enqueued: [11, 12] } as unknown as SyncResult
    expect(resultBreakdown(r)).toBe('再生リスト 12 曲 · Library 9 · Inbox 1 · 新規 2')
    expect(unavailableLine({ position: 31, id: 'x', kind: 'private' })).toBe('#31 非公開の動画 · 飛ばした')
    expect(unavailableLine({ position: 4, id: 'y', kind: 'deleted' })).toBe('#4 削除された動画 · 飛ばした')
  })
  it('sortItems は位置順、位置の無いものは後ろ', () => {
    const xs = [job({ id: 1, payload: {} }), job({ id: 2, payload: { position: 5 } }), job({ id: 3, payload: { position: 2 } })]
    expect(sortItems(xs).map((j) => j.id)).toEqual([3, 2, 1])
  })
})

describe('syncButton', () => {
  it('idle / done は同期、running と応答待ちは無効の同期中、failed は再試行', () => {
    expect(syncButton(null, false)).toEqual({ label: '同期', disabled: false })
    expect(syncButton('idle', false)).toEqual({ label: '同期', disabled: false })
    expect(syncButton('done', false)).toEqual({ label: '同期', disabled: false })
    expect(syncButton('running', false)).toEqual({ label: '同期中', disabled: true })
    expect(syncButton('failed', false)).toEqual({ label: '再試行', disabled: false })
    expect(syncButton('done', true)).toEqual({ label: '同期中', disabled: true })
  })
})
