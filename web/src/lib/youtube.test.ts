import { describe, expect, it } from 'vitest'
import type { Job } from '../api/types'
import {
  parseUrlLines,
  sessionJobs,
  stagedDir,
  stagedDirs,
  urlFromLocation,
  urlRows,
  urlRowsSummary,
  ytdlJobs,
  ytdlResultLabel,
  type LookupItem,
  type PlaylistInfo,
  type PlaylistProbe,
} from './youtube'

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
    expect(ytdlResultLabel(job({ state: 'done' }))).toBe('完了（詳細なし）')
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

describe('urlRows（① の照合。D-87）', () => {
  const V1 = 'https://youtu.be/v1'
  const V2 = 'https://youtu.be/v2'
  const V3 = 'https://youtu.be/v3'
  const PL = 'https://music.youtube.com/playlist?list=PLx'
  const OT = 'https://example.com/a'
  const BAD = 'not a url'
  const lookup = new Map<string, LookupItem>([
    [V1, { url: V1, kind: 'video', video_url: 'w1', located: { location: 'library', path: 'A/01.opus' } }],
    [V2, { url: V2, kind: 'video', video_url: 'w2', located: { location: 'inbox', path: 'youtube/x/a.opus' } }],
    [V3, { url: V3, kind: 'video', video_url: 'w3' }],
    [PL, { url: PL, kind: 'playlist', list_id: 'PLx', subscription: { id: 1, albumartist: 'AA', album: 'AL' } }],
    [OT, { url: OT, kind: 'other' }],
    [BAD, { url: BAD, kind: 'invalid' }],
  ])
  const info = (over: Partial<PlaylistInfo>): PlaylistInfo => ({
    list_id: 'PLx',
    title: 'T',
    entries: 5,
    unavailable: 1,
    in_library: 2,
    in_inbox: 0,
    new: 2,
    truncated: false,
    ...over,
  })

  it('取り込み済みの動画と取れない行は飛ばし、新規と YouTube 以外は投入する', () => {
    const rows = urlRows([V1, V2, V3, OT, BAD], lookup, new Map())
    expect(rows.map((r) => [r.badge, r.skip])).toEqual([
      ['ライブラリにある', true],
      ['Inbox で承認待ち', true],
      ['新規', false],
      ['YouTube 以外', false],
      ['取れない', true],
    ])
    expect(rows[0].detail).toContain('A/01.opus')
  })

  it('照合がまだの行は投入に含める', () => {
    const rows = urlRows(['https://youtu.be/zz'], lookup, new Map())
    expect(rows[0]).toMatchObject({ tone: 'pending', skip: false })
  })

  it('再生リストは列挙の結果で新規の本数を出し、新規が無ければ飛ばす', () => {
    const loading = urlRows([PL], lookup, new Map<string, PlaylistProbe>([[PL, { state: 'loading' }]]))[0]
    expect(loading).toMatchObject({ tone: 'pending', skip: false })
    expect(loading.detail).toContain('購読中（AA / AL）')
    const ok = urlRows([PL], lookup, new Map<string, PlaylistProbe>([[PL, { state: 'ok', info: info({}) }]]))[0]
    expect(ok.skip).toBe(false)
    expect(ok.detail).toContain('5 本のうち新規 2 本（ライブラリに 2・取れない 1 は飛ばす）')
    const none = urlRows([PL], lookup, new Map<string, PlaylistProbe>([[PL, { state: 'ok', info: info({ new: 0 }) }]]))[0]
    expect(none).toMatchObject({ badge: '新規なし', skip: true })
    // 取りこぼしがあれば新規 0 でも飛ばさない（残りに新規があるかもしれない）
    const cut = urlRows([PL], lookup, new Map<string, PlaylistProbe>([[PL, { state: 'ok', info: info({ new: 0, truncated: true }) }]]))[0]
    expect(cut.skip).toBe(false)
    const err = urlRows([PL], lookup, new Map<string, PlaylistProbe>([[PL, { state: 'error', message: 'x' }]]))[0]
    expect(err.skip).toBe(false)
  })

  it('要約は種類ごとの数', () => {
    expect(urlRowsSummary([V1, V3, PL, BAD], lookup)).toBe('4 行（動画 2・再生リスト 1・取れない 1）')
    expect(urlRowsSummary([], lookup)).toBe('')
  })
})

describe('今回の投入の行方（③ ④）', () => {
  it('投入で返ったジョブと、その展開で増えた子だけを古い順に', () => {
    const js = [
      job({ id: 9, payload: { url: 'c2', parent_job_id: 5 } }),
      job({ id: 3 }),
      job({ id: 5, payload: { url: 'pl' } }),
      job({ id: 7, payload: { url: 'other' } }), // 別の投入（購読・別の画面）
      job({ id: 8, payload: { url: 'c1', parent_job_id: 5 } }),
      job({ id: 10, payload: { url: 'x', parent_job_id: 7 } }),
    ]
    expect(sessionJobs(js, [5, 3]).map((j) => j.id)).toEqual([3, 5, 8, 9])
    expect(sessionJobs(js, [])).toEqual([])
    expect(sessionJobs(null, [1])).toEqual([])
  })

  it('Inbox に置いた note から件のディレクトリを取り、件ごとに数える', () => {
    expect(stagedDir(job({ state: 'done', note: 'Inbox に置いた: youtube/Ch/20240101 a [x].opus' }))).toBe('youtube/Ch')
    expect(stagedDir(job({ state: 'done', note: 'Inbox に置いた: a.opus' }))).toBe('')
    expect(stagedDir(job({ state: 'done', note: 'プラグインが skip: x' }))).toBeNull()
    expect(stagedDir(job({ state: 'running', note: null }))).toBeNull()
    expect(
      stagedDirs([
        job({ state: 'done', note: 'Inbox に置いた: youtube/A/1.opus' }),
        job({ state: 'done', note: 'Inbox に置いた: youtube/B/1.opus' }),
        job({ state: 'done', note: 'Inbox に置いた: youtube/A/2.opus' }),
      ]),
    ).toEqual([
      { dir: 'youtube/A', count: 2 },
      { dir: 'youtube/B', count: 1 },
    ])
  })
})
