import { describe, expect, it } from 'vitest'
import type { TrackRow } from '../api/types'
import { BADGE_LEGEND, badgesOf } from './badges'

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
  derived: { opus: null, aac: null },
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
      derived: { opus: { codec: 'opus', stale_tags: true }, aac: null },
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
    // aac 系統だけでは D バッジは出ない（配布ビューは opus。SPEC §7.6）
    const aacOnly = { ...t, derived: { opus: null, aac: { codec: 'aac', stale_tags: false } } }
    expect(badgesOf(aacOnly).find((b) => b.key === 'derived')).toBeUndefined()
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

  it('凡例は badgesOf が出しうるバッジ（key / icon / cls）をすべて含む', () => {
    const legend = new Set(BADGE_LEGEND.flatMap((g) => g.items.map((it) => `${it.key}|${it.icon.replace('•', '')}|${it.cls}`)))
    const hires = { checked_at: 1, stale: false, error: null, cutoff_hz: 22050, cliff_db: 48.3, effective_bits: 16 }
    const rows: TrackRow[] = [
      base,
      { ...base, verification: 'verified_ar' },
      { ...base, verification: 'verified_ctdb' },
      { ...base, verification: 'mismatch' },
      { ...base, verification: 'unverifiable' },
      { ...base, lossless: false, codec: 'opus' },
      { ...base, rg_scanned_at: 1, rg_written_at: 2, rg: { track_gain: 0, track_peak: 0, album_gain: null, album_peak: null } },
      { ...base, rg_scanned_at: 2, rg_written_at: 1, rg: { track_gain: 0, track_peak: 0, album_gain: null, album_peak: null } },
      { ...base, derived: { opus: { codec: 'opus', stale_tags: false }, aac: null } },
      { ...base, derived: { opus: { codec: 'opus', stale_tags: true }, aac: null } },
      { ...base, flac_check: { status: 'md5_missing', checked_at: 1, stale: true, error: null } },
      { ...base, flac_check: { status: 'decode_error', checked_at: 1, stale: false, error: 'x' } },
      { ...base, hires_check: { ...hires, status: 'upsampled' } },
      { ...base, hires_check: { ...hires, status: 'padded' } },
      { ...base, hires_check: { ...hires, status: 'both', stale: true } },
      { ...base, hires_check: { ...hires, status: 'inconclusive' } },
      { ...base, hires_check: { ...hires, status: 'decode_error', error: 'x' } },
      { ...base, pending_batch_id: 1 },
      { ...base, conflict_batch_id: 1 },
      { ...base, duplicate_group: 'ab' },
      { ...base, hardlink: true },
      { ...base, missing_since: 1 },
    ]
    for (const r of rows) {
      for (const b of badgesOf(r)) {
        // stale の • は同じ意味なので凡例では 1 行にまとめる
        const k = `${b.key}|${b.icon.replace('•', '')}|${b.cls}`
        expect(legend.has(k), `凡例に無い: ${k}`).toBe(true)
      }
    }
  })
})
