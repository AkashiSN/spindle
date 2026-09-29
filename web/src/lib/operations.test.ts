import { describe, expect, it } from 'vitest'
import type { Job } from '../api/types'
import {
  embedMessage,
  flaccheckStartedMessage,
  hirescheckStartedMessage,
  verifyStartedMessage,
  mergeVerifyJobs,
  verifyActive,
  verifyResults,
  md5FillMessage,
  operationErrorMessage,
  pathPreviewSummary,
  rgStartedMessage,
  rgWrittenMessage,
  youtubeStartedMessage,
} from './operations'

describe('youtubeStartedMessage（D-70）', () => {
  it('投入したジョブの数と、結果が Inbox に出ることを伝える', () => {
    expect(youtubeStartedMessage({ job_ids: [3, 4] })).toBe(
      'ダウンロードを 2 件投入した（job 3, 4）。進捗はジョブ、結果は Inbox タブ',
    )
    expect(youtubeStartedMessage({ job_ids: [9] })).toBe('ダウンロードを 1 件投入した（job 9）。進捗はジョブ、結果は Inbox タブ')
  })
})

describe('pathPreviewSummary', () => {
  it('変更・変更なし・衝突・反映待ち除外を件数で並べる（0 は省く）', () => {
    expect(
      pathPreviewSummary({ count: 10, changed: 6, unchanged: 3, conflict: 1, pending_excluded: 0 }),
    ).toBe('変更 6 / 変更なし 3 / 衝突 1')
    expect(pathPreviewSummary({ count: 2, changed: 0, unchanged: 0, conflict: 0, pending_excluded: 2 })).toBe(
      '変更 0 / 反映待ちで除外 2',
    )
  })
})

describe('開始・書き込みの結果メッセージ', () => {
  it('RG 解析はアルバム・トラック・重複', () => {
    expect(rgStartedMessage({ albums: 3, tracks: 2, duplicates: 1, job_ids: [] })).toBe(
      'ReplayGain 解析を投入した: アルバム 3 / 単独トラック 2（既に投入済み 1）',
    )
    expect(rgStartedMessage({ albums: 1, tracks: 0, duplicates: 0, job_ids: [1] })).toBe(
      'ReplayGain 解析を投入した: アルバム 1',
    )
  })
  it('FLAC 検査はトラック・対象外・重複', () => {
    expect(flaccheckStartedMessage({ tracks: 5, skipped: 2, duplicates: 0, job_ids: [] })).toBe(
      'FLAC 検査を投入した: 5 件（FLAC でない・欠落で対象外 2）',
    )
  })
  it('偽ハイレゾ検出は件数と対象外・重複', () => {
    expect(hirescheckStartedMessage({ tracks: 2, skipped: 1, duplicates: 0, job_ids: [] })).toBe(
      '偽ハイレゾ検出を投入した: 2 件（対象外（非可逆・44.1/48 kHz かつ 16 bit・欠落） 1）',
    )
    expect(hirescheckStartedMessage({ tracks: 0, skipped: 0, duplicates: 3, job_ids: [] })).toBe(
      '偽ハイレゾ検出を投入した: 0 件（既に投入済み 3）',
    )
  })
  it('遡及照合は何アルバムの照合を始めたかと、結果の出る場所', () => {
    expect(verifyStartedMessage({ albums: 4, duplicates: 0, job_ids: [] })).toBe(
      '4 アルバムの照合を始めた。結果はこの下に出る',
    )
    expect(verifyStartedMessage({ albums: 1, duplicates: 2, job_ids: [] })).toBe(
      '1 アルバムの照合を始めた（2 アルバムは照合待ちなので足さなかった）。結果はこの下に出る',
    )
  })
  it('MD5 補填はバッチ・件数・対象外・反映待ち除外', () => {
    expect(md5FillMessage({ batch_id: 9, affected: 3, skipped: 2, pending_excluded: 1 })).toBe(
      'MD5 の補填を投入した: 3 件（バッチ #9）。対象外 2 / 反映待ちで除外 1',
    )
    expect(md5FillMessage({ batch_id: 9, affected: 1, skipped: 0, pending_excluded: 0 })).toBe(
      'MD5 の補填を投入した: 1 件（バッチ #9）',
    )
  })
  it('RG 書き込みはバッチと内訳。バッチが無ければ既に一致', () => {
    expect(
      rgWrittenMessage({ batch_id: 7, affected: 4, unchanged: 1, unscanned: 2, missing: 0, pending_excluded: 1 }),
    ).toBe('ReplayGain をタグに書く: 4 件（バッチ #7）。既に一致 1 / 未解析 2 / 反映待ちで除外 1')
    expect(
      rgWrittenMessage({ batch_id: null, affected: 0, unchanged: 3, unscanned: 0, missing: 0, pending_excluded: 0 }),
    ).toBe('書く行は無い。既に一致 3')
  })
})

describe('operationErrorMessage', () => {
  it('409 の既知コードを日本語にする', () => {
    expect(operationErrorMessage(409, { error: 'no_changes' })).toBe('対象がありません')
    expect(operationErrorMessage(409, { error: 'preview_stale' })).toBe('プレビューが古くなりました。もう一度プレビューしてください')
    expect(operationErrorMessage(409, { error: 'normalize_disabled' })).toBe('正規化は設定で無効です（[normalize].wav_to_flac）')
    expect(operationErrorMessage(409, { error: 'rg_write_disabled' })).toBe('ReplayGain のタグ書き込みは設定で無効です（[replaygain].write_tags）')
    expect(operationErrorMessage(409, { error: 'md5_fill_disabled' })).toBe('MD5 の補填は設定で無効です（[normalize].flac_fix_missing_md5）')
    expect(operationErrorMessage(503, { error: 'editor_unavailable' })).toBe('編集機能が使えません（読み取り専用で起動している）')
  })
  it('未知のコードは message かコードと HTTP 状態', () => {
    expect(operationErrorMessage(400, { error: 'bad_request', message: 'x が不正' })).toBe('x が不正')
    expect(operationErrorMessage(500, null)).toBe('http_error (HTTP 500)')
  })
})

describe('embedMessage', () => {
  it('埋め込み画像の差し替えはバッチ・件数・既に同じ・欠落・反映待ち除外', () => {
    expect(embedMessage({ batch_id: 4, affected: 12, unchanged: 3, missing: 1, pending_excluded: 2 })).toBe(
      '埋め込み画像の差し替えを投入した: 12 件（バッチ #4）。既に同じ画像 3 / 欠落 1 / 反映待ちで除外 2',
    )
    expect(embedMessage({ batch_id: 4, affected: 1, unchanged: 0, missing: 0, pending_excluded: 0 })).toBe(
      '埋め込み画像の差し替えを投入した: 1 件（バッチ #4）',
    )
  })
  it('画像まわりのエラーコードは日本語', () => {
    expect(operationErrorMessage(404, { error: 'artwork_not_found' })).toBe('画像が登録されていません。もう一度アップロードしてください')
    expect(operationErrorMessage(400, { error: 'unsupported_image' })).toBe('JPEG / PNG / WebP の画像だけを受け付けます')
    expect(operationErrorMessage(503, { error: 'artwork_unavailable' })).toBe('アートワークのキャッシュが無いので画像を扱えません')
  })
})

describe('verifyResults（遡及照合の結果を操作タブに出す）', () => {
  const job = (over: Partial<Job>): Job => ({
    id: 1,
    type: 'verify',
    state: 'queued',
    progress: null,
    done: null,
    total: null,
    attempts: 0,
    max_attempts: 5,
    last_error: null,
    run_after: null,
    edit_batch_id: null,
    created_at: 0,
    started_at: null,
    finished_at: null,
    note: null,
    subject: 'A/Alb',
    ...over,
  })
  const seen = (...jobs: Job[]) => new Map(jobs.map((j) => [j.id, j]))

  it('終わったジョブは対象と結果 1 行、終わっていなければ待ちの数', () => {
    const r = verifyResults(
      seen(
        job({ id: 1, state: 'done', note: 'CTDB 全 2 曲一致（信頼度 34） / AccurateRip 登録なし' }),
        job({ id: 2, state: 'running', subject: 'B/Alb' }),
      ),
      [1, 2],
      true,
    )
    expect(r.pending).toBe(1)
    expect(r.lines).toEqual([
      { id: 1, subject: 'A/Alb', text: 'CTDB 全 2 曲一致（信頼度 34） / AccurateRip 登録なし', kind: 'done' },
    ])
  })

  it('失敗・取り消し・再試行待ち', () => {
    const r = verifyResults(
      seen(
        job({ id: 1, state: 'failed', last_error: 'CTDB の照会: timeout' }),
        job({ id: 2, state: 'cancelled', subject: null }),
        job({ id: 3, state: 'queued', attempts: 1, last_error: 'AccurateRip の照会: 503' }),
      ),
      [1, 2, 3],
      true,
    )
    expect(r.pending).toBe(1)
    expect(r.lines).toEqual([
      { id: 1, subject: 'A/Alb', text: '失敗: CTDB の照会: timeout', kind: 'error' },
      { id: 2, subject: 'job #2', text: '取り消した', kind: 'error' },
      { id: 3, subject: 'A/Alb', text: '再試行待ち: AccurateRip の照会: 503', kind: 'pending' },
    ])
  })

  it('一覧に出ていないジョブは、verify が動いている間は待ち（一覧は状態ごとに件数の上限がある）', () => {
    expect(verifyResults(seen(), [1, 2], true)).toEqual({ pending: 2, lines: [] })
    // verify が 1 件も動いていないのに一度も見えなかった（片付けられたか上限の外）ものは取れない
    expect(verifyResults(seen(job({ id: 1, state: 'done', note: 'x' })), [1, 2], false)).toEqual({
      pending: 0,
      lines: [
        { id: 1, subject: 'A/Alb', text: 'x', kind: 'done' },
        { id: 2, subject: 'job #2', text: '結果を取れない（ジョブ画面で確認）', kind: 'error' },
      ],
    })
  })

  it('待ち・実行中として見えたまま verify が 1 件も動かなくなったら（完了の上限の外へ押し出された）取れない', () => {
    const r = verifyResults(seen(job({ id: 1, state: 'queued' }), job({ id: 2, state: 'running', subject: 'B/Alb' })), [1, 2], false)
    expect(r).toEqual({
      pending: 0,
      lines: [
        { id: 1, subject: 'A/Alb', text: '結果を取れない（ジョブ画面で確認）', kind: 'error' },
        { id: 2, subject: 'B/Alb', text: '結果を取れない（ジョブ画面で確認）', kind: 'error' },
      ],
    })
  })

  it('結果の無い完了は「完了」', () => {
    const r = verifyResults(seen(job({ id: 1, state: 'done', note: null })), [1], false)
    expect(r).toEqual({ pending: 0, lines: [{ id: 1, subject: 'A/Alb', text: '完了', kind: 'done' }] })
  })
})

describe('mergeVerifyJobs', () => {
  const job = (id: number, state: Job['state']): Job =>
    ({ id, type: 'verify', state, subject: null }) as Job

  it('追っている id だけを新しい状態で上書きし、一覧から落ちたものは前の状態を残す', () => {
    const prev = new Map([
      [1, job(1, 'done')],
      [2, job(2, 'queued')],
    ])
    const next = mergeVerifyJobs(prev, [job(2, 'running'), job(9, 'done')], new Set([1, 2, 3]))
    expect([...next.entries()].map(([id, j]) => [id, j.state])).toEqual([
      [1, 'done'],
      [2, 'running'],
    ])
    expect(prev.get(2)!.state).toBe('queued')
  })
})

describe('verifyActive', () => {
  it('verify の待ちか実行中が 1 件でもあれば true', () => {
    const counts = (queued: number, running: number) => ({ queued, running, done: 0, failed: 0, cancelled: 0 })
    expect(verifyActive({ verify: counts(0, 1) })).toBe(true)
    expect(verifyActive({ verify: counts(2, 0) })).toBe(true)
    expect(verifyActive({ verify: counts(0, 0) })).toBe(false)
    expect(verifyActive({ rg: counts(5, 5) })).toBe(false)
  })
})
