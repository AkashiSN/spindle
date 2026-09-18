import { describe, expect, it } from 'vitest'
import type { TrackDetail, TrackRow } from '../api/types'
import {
  commonValue,
  generalRows,
  joinValues,
  locationRows,
  metadataRows,
  splitValues,
  STANDARD_KEYS,
} from './properties'

function row(over: Partial<TrackRow> = {}): TrackRow {
  return {
    id: 1,
    title: 't',
    artist_display: 'a',
    album: 'al',
    albumartist: 'aa',
    track_no: 1,
    disc_no: 1,
    date: '2020',
    category: null,
    duration_ms: 60_000,
    codec: 'flac',
    lossless: true,
    verification: 'not_attempted',
    rg_scanned_at: null,
    rg_written_at: null,
    rg: null,
    derived: null,
    flac_check: null,
    pending_batch_id: null,
    conflict_batch_id: null,
    duplicate_group: null,
    hardlink: false,
    missing_since: null,
    rel_path: 'Library/A/x.flac',
    album_id: null,
    artwork_hash: null,
    ...over,
  }
}

function detail(over: Partial<TrackDetail> = {}): TrackDetail {
  return {
    tags: { TITLE: ['t'], ARTIST: ['a'] },
    size: 1000,
    mtime: 1_700_000_000,
    sample_rate: 44100,
    bit_depth: 16,
    channels: 2,
    bitrate: 900,
    audio_md5: null,
    original_codec: null,
    added_at: 1_690_000_000,
    ...over,
  }
}

const byKey = (rows: { key: string }[]) => rows.map((r) => r.key)
const valueOf = (rows: { key: string; value: unknown }[], key: string) => rows.find((r) => r.key === key)?.value

describe('commonValue', () => {
  it('全部同じなら text、違えば multiple、全部空なら empty', () => {
    expect(commonValue(['x', 'x'])).toEqual({ kind: 'text', text: 'x' })
    expect(commonValue(['x', 'y'])).toEqual({ kind: 'multiple' })
    expect(commonValue(['', null, undefined])).toEqual({ kind: 'empty' })
    expect(commonValue([])).toEqual({ kind: 'empty' })
  })
  it('片方だけ空でも multiple（欠けているのは差異）', () => {
    expect(commonValue(['x', ''])).toEqual({ kind: 'multiple' })
  })
})

describe('joinValues / splitValues', () => {
  it('多値は "; " で結び、編集時は ";" で割って空を落とす', () => {
    expect(joinValues(['A', 'B'])).toBe('A; B')
    expect(splitValues('A; B ;; C')).toEqual(['A', 'B', 'C'])
    expect(splitValues('   ')).toEqual([])
  })
})

describe('metadataRows', () => {
  it('標準キーは空でも固定順で出し、その他のキーは値があるものだけ後ろにアルファベット順（PICTURE は出さない）', () => {
    const rows = metadataRows([detail({ tags: { TITLE: ['t'], ZZZ: ['1'], MUSICBRAINZ_TRACKID: ['m'], PICTURE: ['image/jpeg:ab'] } })])
    const keys = byKey(rows)
    expect(keys.slice(0, STANDARD_KEYS.length)).toEqual(STANDARD_KEYS)
    expect(keys.slice(STANDARD_KEYS.length)).toEqual(['MUSICBRAINZ_TRACKID', 'ZZZ'])
    expect(valueOf(rows, 'TITLE')).toEqual({ kind: 'text', text: 't' })
    expect(valueOf(rows, 'ALBUM')).toEqual({ kind: 'empty' })
  })
  it('複数選択は共通値、違えば multiple。多値は結合して比べる', () => {
    const rows = metadataRows([
      detail({ tags: { ARTIST: ['A', 'B'], ALBUM: ['x'], COMMENT: ['c'] } }),
      detail({ tags: { ARTIST: ['A', 'B'], ALBUM: ['y'] } }),
    ])
    expect(valueOf(rows, 'ARTIST')).toEqual({ kind: 'text', text: 'A; B' })
    expect(valueOf(rows, 'ALBUM')).toEqual({ kind: 'multiple' })
    expect(valueOf(rows, 'COMMENT')).toEqual({ kind: 'multiple' })
  })
  it('詳細が 1 件も無ければ標準キーだけ空で出す', () => {
    const rows = metadataRows([])
    expect(byKey(rows)).toEqual(STANDARD_KEYS)
    expect(rows.every((r) => r.value.kind === 'empty')).toBe(true)
  })
})

describe('locationRows', () => {
  it('1 件: パス・フォルダ・ファイル名・サイズ・更新・追加', () => {
    const rows = locationRows([row()], new Map([[1, detail()]]))
    expect(valueOf(rows, 'path')).toEqual({ kind: 'text', text: 'Library/A/x.flac' })
    expect(valueOf(rows, 'folder')).toEqual({ kind: 'text', text: 'Library/A' })
    expect(valueOf(rows, 'name')).toEqual({ kind: 'text', text: 'x.flac' })
    expect(valueOf(rows, 'size')).toEqual({ kind: 'text', text: '1,000 B' })
    expect((valueOf(rows, 'mtime') as { text: string }).text).toMatch(/^\d{4}-\d{2}-\d{2} \d{2}:\d{2}$/)
  })
  it('複数: 共通のフォルダは出し、サイズは合計（詳細が届いた分だけ）', () => {
    const rows = locationRows(
      [row({ id: 1 }), row({ id: 2, rel_path: 'Library/A/y.flac' }), row({ id: 3, rel_path: 'Library/A/z.flac' })],
      new Map([
        [1, detail({ size: 1000 })],
        [2, detail({ size: 2000 })],
      ]),
    )
    expect(valueOf(rows, 'path')).toEqual({ kind: 'multiple' })
    expect(valueOf(rows, 'folder')).toEqual({ kind: 'text', text: 'Library/A' })
    expect(valueOf(rows, 'size')).toEqual({ kind: 'text', text: '3,000 B（2 / 3 件）' })
  })
})

describe('generalRows', () => {
  it('技術情報は単位付き、長さは複数なら合計', () => {
    const rows = generalRows([row()], new Map([[1, detail({ audio_md5: 'ab', original_codec: 'wav' })]]))
    expect(valueOf(rows, 'duration')).toEqual({ kind: 'text', text: '1:00' })
    expect(valueOf(rows, 'codec')).toEqual({ kind: 'text', text: 'FLAC（可逆）' })
    expect(valueOf(rows, 'sample_rate')).toEqual({ kind: 'text', text: '44100 Hz' })
    expect(valueOf(rows, 'bit_depth')).toEqual({ kind: 'text', text: '16 bit' })
    expect(valueOf(rows, 'channels')).toEqual({ kind: 'text', text: '2' })
    expect(valueOf(rows, 'bitrate')).toEqual({ kind: 'text', text: '900 kbps' })
    expect(valueOf(rows, 'audio_md5')).toEqual({ kind: 'text', text: 'ab' })
    expect(valueOf(rows, 'original_codec')).toEqual({ kind: 'text', text: 'wav' })
    expect(valueOf(rows, 'verification')).toEqual({ kind: 'text', text: '未検証' })
    expect(valueOf(rows, 'rg')).toEqual({ kind: 'empty' })
    expect(valueOf(rows, 'flac_check')).toEqual({ kind: 'empty' })
    expect(valueOf(rows, 'state')).toEqual({ kind: 'empty' })

    const two = generalRows([row({ id: 1 }), row({ id: 2, duration_ms: 30_000, codec: 'opus', lossless: false })], new Map())
    expect(valueOf(two, 'duration')).toEqual({ kind: 'text', text: '1:30（2 件）' })
    expect(valueOf(two, 'codec')).toEqual({ kind: 'multiple' })
    expect(valueOf(two, 'sample_rate')).toEqual({ kind: 'empty' })
  })
  it('RG・FLAC 検査・Derived・状態を 1 行ずつにまとめる', () => {
    const rows = generalRows(
      [
        row({
          rg: { track_gain: -6.5, track_peak: 0.98, album_gain: -7, album_peak: 1 },
          rg_scanned_at: 1_700_000_000,
          rg_written_at: null,
          flac_check: { status: 'decode_error', checked_at: 1_700_000_000, stale: false, error: 'boom' },
          derived: { codec: 'opus', stale_tags: true },
          pending_batch_id: 5,
          duplicate_group: 'dead',
          hardlink: true,
          missing_since: 1_700_000_000,
        }),
      ],
      new Map(),
    )
    expect(valueOf(rows, 'rg')).toEqual({ kind: 'text', text: 'track -6.50 dB / peak 0.980000, album -7.00 dB / peak 1.000000（未書き込み）' })
    expect(valueOf(rows, 'flac_check')).toEqual({ kind: 'text', text: 'デコードエラー: boom' })
    expect(valueOf(rows, 'derived')).toEqual({ kind: 'text', text: 'opus（タグが古い）' })
    expect(valueOf(rows, 'state')).toEqual({ kind: 'text', text: '反映待ち #5, 重複, hardlink, 欠落' })
  })
})
