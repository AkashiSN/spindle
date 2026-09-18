import { describe, expect, it } from 'vitest'
import type { TrackRow } from '../api/types'
import { badgesOf } from './badges'

const base: TrackRow = {
  id: 1,
  title: 't',
  artist_display: null,
  album: null,
  albumartist: null,
  track_no: 1,
  disc_no: 1,
  date: null,
  category: null,
  duration_ms: 1000,
  codec: 'flac',
  lossless: true,
  verification: 'not_attempted',
  rg_scanned_at: null,
  rg: null,
  rg_written_at: null,
  derived: null,
  flac_check: null,
  album_id: null,
  pending_batch_id: null,
  conflict_batch_id: null,
  duplicate_group: null,
  hardlink: false,
  missing_since: null,
  rel_path: 'a.flac',
}

const keys = (t: TrackRow) => badgesOf(t).map((b) => b.key)

describe('badgesOf', () => {
  it('検証と可逆は常に出る。unverifiable は未検証と別の見え方', () => {
    expect(keys(base)).toEqual(['verification', 'lossless'])
    const none = badgesOf(base)[0]
    const unv = badgesOf({ ...base, verification: 'unverifiable' })[0]
    expect(none.cls).not.toBe(unv.cls)
    expect(none.icon).not.toBe(unv.icon)
  })

  it('RG は計測のみなら半透明、書き込み済みなら通常', () => {
    const scanned = badgesOf({ ...base, rg_scanned_at: 10 }).find((b) => b.key === 'rg')!
    expect(scanned.cls).toContain('b-faded')
    const stale = badgesOf({ ...base, rg_scanned_at: 10, rg_written_at: 5 }).find((b) => b.key === 'rg')!
    expect(stale.cls).toContain('b-faded')
    const written = badgesOf({ ...base, rg_scanned_at: 10, rg_written_at: 10 }).find((b) => b.key === 'rg')!
    expect(written.cls).not.toContain('b-faded')
  })

  it('Derived の stale_tags は点付き、pending / conflict / 重複 / hardlink / missing が並ぶ', () => {
    const t: TrackRow = {
      ...base,
      derived: { codec: 'opus', stale_tags: true },
      pending_batch_id: 42,
      conflict_batch_id: 41,
      duplicate_group: 'ab',
      hardlink: true,
      missing_since: 1,
    }
    expect(keys(t)).toEqual([
      'verification',
      'lossless',
      'derived',
      'pending',
      'conflict',
      'dup',
      'hardlink',
      'missing',
    ])
    expect(badgesOf(t).find((b) => b.key === 'derived')!.icon).toBe('D•')
    expect(badgesOf(t).find((b) => b.key === 'pending')!.label).toContain('#42')
  })

  it('FLAC 健全性チェックは decode_error と md5_missing のときだけ出し、古い結果は点付き', () => {
    const ok: TrackRow = { ...base, flac_check: { status: 'ok', checked_at: 1, stale: false, error: null } }
    expect(keys(ok)).not.toContain('flac')
    const bad: TrackRow = {
      ...base,
      flac_check: { status: 'decode_error', checked_at: 1, stale: false, error: 'boom' },
    }
    const b = badgesOf(bad).find((x) => x.key === 'flac')!
    expect(b.icon).toBe('✘F')
    expect(b.cls).toContain('b-flac-error')
    expect(b.label).toContain('boom')
    const nomd5: TrackRow = {
      ...base,
      flac_check: { status: 'md5_missing', checked_at: 1, stale: true, error: null },
    }
    const m = badgesOf(nomd5).find((x) => x.key === 'flac')!
    expect(m.icon).toBe('F•')
    expect(m.cls).toContain('b-flac-md5')
    expect(m.label).toContain('MD5')
    expect(m.label).toContain('古い')
  })
})
