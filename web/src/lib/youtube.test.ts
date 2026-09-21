import { describe, expect, it } from 'vitest'
import type { Job } from '../api/types'
import { parseUrlLines, urlFromLocation, ytdlJobs, ytdlResultLabel } from './youtube'

function job(over: Partial<Job>): Job {
  return {
    id: 1,
    type: 'ytdl',
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
    subject: 'https://www.youtube.com/watch?v=a',
    ...over,
  }
}

describe('parseUrlLines', () => {
  it('空行と重複を落として順序を保つ', () => {
    expect(parseUrlLines(' https://youtu.be/a \n\nhttps://youtu.be/b\r\nhttps://youtu.be/a\n')).toEqual([
      'https://youtu.be/a',
      'https://youtu.be/b',
    ])
    expect(parseUrlLines('\n  \n')).toEqual([])
  })
})

describe('urlFromLocation（/youtube?url= の受け口）', () => {
  it('/youtube の url クエリだけを返す', () => {
    expect(urlFromLocation('/youtube', '?url=https%3A%2F%2Fwww.youtube.com%2Fwatch%3Fv%3Dabc')).toBe(
      'https://www.youtube.com/watch?v=abc',
    )
    expect(urlFromLocation('/youtube/', '?url=https%3A%2F%2Fyoutu.be%2Fx&other=1')).toBe('https://youtu.be/x')
    expect(urlFromLocation('/youtube', '')).toBeNull()
    expect(urlFromLocation('/youtube', '?url=')).toBeNull()
    expect(urlFromLocation('/', '?url=https%3A%2F%2Fyoutu.be%2Fx')).toBeNull()
    // http / https 以外は受けない（javascript: 等）
    expect(urlFromLocation('/youtube', '?url=javascript%3Aalert(1)')).toBeNull()
  })
})

describe('ytdlJobs', () => {
  it('ytdl だけを新しい順に', () => {
    const items = [
      job({ id: 1, created_at: 10 }),
      job({ id: 2, type: 'transcode', created_at: 20 }),
      job({ id: 3, created_at: 30 }),
      job({ id: 4, created_at: 30 }),
    ]
    expect(ytdlJobs(items).map((j) => j.id)).toEqual([4, 3, 1])
  })
})

describe('ytdlResultLabel', () => {
  it('状態と結果を 1 行に', () => {
    expect(ytdlResultLabel(job({ state: 'queued' }))).toBe('待ち')
    expect(ytdlResultLabel(job({ state: 'running' }))).toBe('ダウンロード中')
    // 完了はサーバの note（Staged / Skipped / Playlist を区別する）。無ければ断定しない
    expect(ytdlResultLabel(job({ state: 'done', note: 'Inbox に置いた: youtube/A/x.opus' }))).toBe(
      'Inbox に置いた: youtube/A/x.opus',
    )
    expect(ytdlResultLabel(job({ state: 'done', note: 'プラグインが skip: 告知動画' }))).toBe('プラグインが skip: 告知動画')
    expect(
      ytdlResultLabel(job({ state: 'done', note: '再生リストを展開した: 1 件を投入、224 件は取り込み済み' })),
    ).toBe('再生リストを展開した: 1 件を投入、224 件は取り込み済み')
    expect(ytdlResultLabel(job({ state: 'done' }))).toBe('完了（結果は Inbox）')
    expect(ytdlResultLabel(job({ state: 'running', subject: 'https://www.youtube.com/playlist?list=PL1' }))).toBe(
      '再生リストを展開中',
    )
    expect(ytdlResultLabel(job({ state: 'failed', last_error: '取り込み済み（Library）: A/01.opus' }))).toBe(
      '取り込み済み（Library）: A/01.opus',
    )
    expect(ytdlResultLabel(job({ state: 'failed', attempts: 2, last_error: null }))).toBe('失敗')
    expect(ytdlResultLabel(job({ state: 'queued', attempts: 1, last_error: 'timeout', run_after: 5 }))).toBe(
      '再試行待ち（timeout）',
    )
    expect(ytdlResultLabel(job({ state: 'cancelled' }))).toBe('取り消し')
  })
})
