import { describe, expect, it } from 'vitest'
import { splitValues } from '../lib/properties'
import { audioSpec, editBlockReason, fallbackSource, formatSize, mobileSource, sourceCodecLabel, sheetFromHistory, tagEditOps, tagEditProblem } from './track'

const yes = { flac: true, opus: true, aac: true, alac: true, wav: true, mp3: true }

describe('track', () => {
  it('可逆は Opus、Opus が鳴らない端末は AAC、非可逆は原本', () => {
    expect(mobileSource({ id: 7, lossless: true }, yes)).toEqual({ url: '/api/stream/7?transcode=opus', transcode: true })
    expect(mobileSource({ id: 7, lossless: true }, { ...yes, opus: false })).toEqual({
      url: '/api/stream/7?transcode=aac',
      transcode: true,
    })
    expect(mobileSource({ id: 8, lossless: false }, yes)).toEqual({ url: '/api/stream/8', transcode: false })
  })

  it('反映待ち・競合・missing は編集させない', () => {
    const ok = { pending_batch_id: null, conflict_batch_id: null, missing_since: null }
    expect(editBlockReason(ok)).toBeNull()
    expect(editBlockReason({ ...ok, pending_batch_id: 3 })).toBe('反映待ちの間は編集できません')
    expect(editBlockReason({ ...ok, conflict_batch_id: 4 })).toBe('競合を PC で解決してから編集してください')
    expect(editBlockReason({ ...ok, missing_since: 1 })).toBe('ファイルが見つかりません')
  })

  it('tagEditOps はキーを大文字にし、多値を splitValues で分ける', () => {
    expect(tagEditOps('artist', 'A; B')).toEqual([{ op: 'set', key: 'ARTIST', value: splitValues('A; B') }])
  })

  it('空の値は保存させない（フィールドの削除はスマホ版ではしない）', () => {
    expect(tagEditProblem('  ;  ')).toBe('空にはできません。フィールドの削除は PC で行ってください')
    expect(tagEditProblem('A')).toBeNull()
  })

  it('history.state から開いているシートを読む', () => {
    expect(sheetFromHistory(null)).toBeNull()
    expect(sheetFromHistory({})).toBeNull()
    expect(sheetFromHistory({ sheet: 'x' })).toBeNull()
    expect(sheetFromHistory({ sheet: 0 })).toBeNull()
    expect(sheetFromHistory({ sheet: 5 })).toEqual({ trackId: 5, editKey: null })
    expect(sheetFromHistory({ sheet: 5, edit: 'TITLE' })).toEqual({ trackId: 5, editKey: 'TITLE' })
    expect(sheetFromHistory({ sheet: 5, edit: 3 })).toEqual({ trackId: 5, editKey: null })
  })

  it('ビット深度 / 周波数のバッジ', () => {
    expect(audioSpec({ bit_depth: 24, sample_rate: 96000 })).toBe('24/96kHz')
    expect(audioSpec({ bit_depth: 16, sample_rate: 44100 })).toBe('16/44.1kHz')
    expect(audioSpec({ bit_depth: null, sample_rate: 48000 })).toBe('48kHz')
    expect(audioSpec({ bit_depth: null, sample_rate: null })).toBeNull()
  })

  it('サイズは KB / MB / GB で丸める', () => {
    expect(formatSize(512)).toBe('512 B')
    expect(formatSize(2048)).toBe('2.0 KB')
    expect(formatSize(34_200_000)).toBe('32.6 MB')
    expect(formatSize(3 * 1024 ** 3)).toBe('3.00 GB')
  })

  it('Opus で鳴らなかった可逆は 1 回だけ AAC に倒す', () => {
    const opus = { url: '/api/stream/7?transcode=opus', transcode: true }
    const aac = { url: '/api/stream/7?transcode=aac', transcode: true }
    expect(fallbackSource({ id: 7, lossless: true }, opus)).toEqual(aac)
    expect(fallbackSource({ id: 7, lossless: true }, aac)).toBeNull()
    expect(fallbackSource({ id: 8, lossless: false }, { url: '/api/stream/8', transcode: false })).toBeNull()
  })

  it('鳴らしている形式の表示', () => {
    expect(sourceCodecLabel({ url: '/api/stream/7?transcode=opus', transcode: true })).toBe('Opus')
    expect(sourceCodecLabel({ url: '/api/stream/7?transcode=aac', transcode: true })).toBe('AAC')
    expect(sourceCodecLabel({ url: '/api/stream/8', transcode: false })).toBe('原本')
  })
})
