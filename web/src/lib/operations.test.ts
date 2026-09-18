import { describe, expect, it } from 'vitest'
import {
  embedMessage,
  flaccheckStartedMessage,
  md5FillMessage,
  operationErrorMessage,
  pathPreviewSummary,
  rgStartedMessage,
  rgWrittenMessage,
} from './operations'

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
