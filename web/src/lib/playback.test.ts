import { describe, expect, it } from 'vitest'
import type { TrackRow } from '../api/types'
import {
  detectSupport,
  formatTime,
  nextTrack,
  prevTrack,
  resolvePendingSeek,
  rgGain,
  streamUrl,
  withStart,
  type Support,
} from './playback'

const ALL: Support = { flac: true, opus: true, aac: true, alac: true, wav: true, mp3: true }
const CHROME: Support = { ...ALL, alac: false }

function row(id: number, codec: string, lossless: boolean, missing = false): TrackRow {
  return {
    id,
    title: null,
    artist_display: null,
    album: null,
    albumartist: null,
    track_no: null,
    disc_no: null,
    date: null,
    category: null,
    duration_ms: null,
    codec,
    lossless,
    verification: 'not_attempted',
    rg_scanned_at: null,
    rg_written_at: null,
    rg: null,
    derived: null,
    flac_check: null,
    album_id: null,
    artwork_hash: null,
    pending_batch_id: null,
    conflict_batch_id: null,
    duplicate_group: null,
    hardlink: false,
    missing_since: missing ? 1 : null,
    rel_path: `${id}.${codec}`,
  }
}

describe('detectSupport', () => {
  it('probably / maybe を再生可、空文字を不可とみなす', () => {
    const s = detectSupport((m) => (m.startsWith('audio/flac') ? 'probably' : m.includes('opus') ? 'maybe' : ''))
    expect(s.flac).toBe(true)
    expect(s.opus).toBe(true)
    expect(s.alac).toBe(false)
    expect(s.mp3).toBe(false)
  })
})

describe('streamUrl', () => {
  it('可逆は既定で Derived の Opus、原本を選べば直送', () => {
    expect(streamUrl(row(1, 'flac', true), CHROME, false)).toEqual({ url: '/api/stream/1?transcode=opus', transcode: true })
    expect(streamUrl(row(1, 'flac', true), CHROME, true)).toEqual({ url: '/api/stream/1', transcode: false })
  })
  it('再生できない codec は原本を選んでいても変換する', () => {
    expect(streamUrl(row(2, 'alac', true), CHROME, true).transcode).toBe(true)
    expect(streamUrl(row(2, 'alac', true), ALL, true).transcode).toBe(false)
  })
  it('非可逆は常に原本（多重劣化の回避）', () => {
    expect(streamUrl(row(3, 'opus', false), CHROME, false)).toEqual({ url: '/api/stream/3', transcode: false })
    expect(streamUrl(row(3, 'mp3', false), { ...CHROME, mp3: false }, false).transcode).toBe(false)
  })
  it('Opus も再生できなければ原本を試す', () => {
    expect(streamUrl(row(4, 'flac', true), { ...ALL, opus: false }, false).transcode).toBe(false)
  })
  it('withStart は変換ストリームだけに付く', () => {
    expect(withStart({ url: '/api/stream/1?transcode=opus', transcode: true }, 12.5)).toBe(
      '/api/stream/1?transcode=opus&start=12.500',
    )
    expect(withStart({ url: '/api/stream/1?transcode=opus', transcode: true }, 0)).toBe('/api/stream/1?transcode=opus')
    expect(withStart({ url: '/api/stream/1', transcode: false }, 12.5)).toBe('/api/stream/1')
  })
})

describe('rgGain', () => {
  it('dB → 線形、peak で抑える', () => {
    expect(rgGain({ track_gain: -6.0206, track_peak: 0.5, album_gain: null, album_peak: null }, true)).toBeCloseTo(0.5, 4)
    // +6 dB だが peak 0.9 なので 1/0.9 に抑える
    expect(rgGain({ track_gain: 6, track_peak: 0.9, album_gain: null, album_peak: null }, true)).toBeCloseTo(1 / 0.9, 6)
    expect(rgGain({ track_gain: 6, track_peak: 0, album_gain: null, album_peak: null }, true)).toBeCloseTo(1.9953, 3)
  })
  it('無効・未解析は 1', () => {
    expect(rgGain(null, true)).toBe(1)
    expect(rgGain({ track_gain: -6, track_peak: 0.5, album_gain: null, album_peak: null }, false)).toBe(1)
  })
})

describe('nextTrack', () => {
  const rows = [row(1, 'flac', true), row(2, 'flac', true, true), row(3, 'opus', false), row(5, 'flac', true)]
  it('表の順で次の行。missing は飛ばす', () => {
    expect(nextTrack(rows, 1, true)).toBe(rows[2])
  })
  it('末尾で未読があれば unloaded、読み切っていれば null', () => {
    expect(nextTrack(rows, 5, false)).toBe('unloaded')
    expect(nextTrack(rows, 5, true)).toBe(null)
  })
  it('現在曲が表に無ければ null', () => {
    expect(nextTrack(rows, 99, false)).toBe(null)
  })
})

describe('prevTrack', () => {
  const rows = [row(1, 'flac', true), row(2, 'flac', true, true), row(3, 'opus', false), row(5, 'flac', true)]
  it('表の順で前の行。missing は飛ばす。先頭や表に無ければ null', () => {
    expect(prevTrack(rows, 3)).toBe(rows[0])
    expect(prevTrack(rows, 5)).toBe(rows[2])
    expect(prevTrack(rows, 1)).toBe(null)
    expect(prevTrack(rows, 99)).toBe(null)
  })
})

describe('formatTime', () => {
  it('m:ss / h:mm:ss', () => {
    expect(formatTime(0)).toBe('0:00')
    expect(formatTime(65.9)).toBe('1:05')
    expect(formatTime(3725)).toBe('1:02:05')
    expect(formatTime(NaN)).toBe('0:00')
    expect(formatTime(-1)).toBe('0:00')
  })
})

describe('resolvePendingSeek', () => {
  it('保留が無ければ何もしない', () => {
    expect(resolvePendingSeek(true, null, 0)).toEqual({ kind: 'none' })
    expect(resolvePendingSeek(false, null, 100)).toEqual({ kind: 'none' })
  })
  it('Range でシークできるなら offset を引いた currentTime', () => {
    expect(resolvePendingSeek(true, 30, 0)).toEqual({ kind: 'native', currentTime: 30 })
    expect(resolvePendingSeek(true, 30, 10)).toEqual({ kind: 'native', currentTime: 20 })
    expect(resolvePendingSeek(true, 5, 10)).toEqual({ kind: 'native', currentTime: 0 })
  })
  it('chunked は前方も後方も start= で読み直す。実質同じ位置なら何もしない', () => {
    expect(resolvePendingSeek(false, 30, 0)).toEqual({ kind: 'reload', start: 30 })
    // start=100 で読み直し中に 20 秒へ（後方）
    expect(resolvePendingSeek(false, 20, 100)).toEqual({ kind: 'reload', start: 20 })
    expect(resolvePendingSeek(false, 100.5, 100)).toEqual({ kind: 'none' })
    expect(resolvePendingSeek(false, -3, 0)).toEqual({ kind: 'none' })
  })
})
