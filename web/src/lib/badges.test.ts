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
  hires_check: null,
  album_id: null,
  artwork_hash: null,
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

  it('偽ハイレゾ検出は疑いと inconclusive だけバッジを出し、stale は • を付ける', () => {
    const ok = {
      status: 'ok' as const,
      checked_at: 1,
      stale: false,
      error: null,
      cutoff_hz: 48000,
      cliff_db: null,
      effective_bits: 24,
    }
    expect(keys({ ...base, hires_check: ok })).toEqual(['verification', 'lossless'])
    const up = badgesOf({ ...base, hires_check: { ...ok, status: 'upsampled', cutoff_hz: 22050, cliff_db: 48.3 } })
    const b = up.find((x) => x.key === 'hires')!
    expect(b.icon).toBe('H')
    expect(b.cls).toContain('b-hires-suspect')
    expect(b.label).toContain('22.1 kHz')
    expect(b.label).toContain('48 dB')
    const both = badgesOf({
      ...base,
      hires_check: { ...ok, status: 'both', stale: true, cutoff_hz: 22050, cliff_db: 48.3, effective_bits: 16 },
    })
    const bb = both.find((x) => x.key === 'hires')!
    expect(bb.icon).toBe('H•')
    expect(bb.label).toContain('16 bit')
    expect(bb.label).toContain('結果が古い')
    const inc = badgesOf({ ...base, hires_check: { ...ok, status: 'inconclusive', cutoff_hz: 23000, cliff_db: 12 } })
    expect(inc.find((x) => x.key === 'hires')!.cls).toContain('b-hires-inconclusive')
    const err = badgesOf({
      ...base,
      hires_check: { ...ok, status: 'decode_error', error: 'boom', cutoff_hz: null, effective_bits: null },
    })
    expect(err.find((x) => x.key === 'hires')!.label).toContain('boom')
  })
})
