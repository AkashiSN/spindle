import { describe, expect, it } from 'vitest'
import { archiveStateLabel, formatBytes, gcPreviewRows } from './settings'

describe('formatBytes', () => {
  it('1024 区切りで単位を付ける', () => {
    expect(formatBytes(0)).toBe('0 B')
    expect(formatBytes(900)).toBe('900 B')
    expect(formatBytes(1536)).toBe('1.5 KB')
    expect(formatBytes(30 * 1024 * 1024 * 1024)).toBe('30.0 GB')
  })
})

describe('gcPreviewRows', () => {
  it('区分ごとに件数・バイト・先頭のサンプルを並べる（順序固定）', () => {
    const section = (count: number, bytes: number, sample: string[]) => ({ count, bytes, sample })
    const rows = gcPreviewRows({
      now: 1,
      cutoff: 0,
      tracks: section(2, 0, ['a', 'b']),
      albums: section(0, 0, []),
      archived: section(1, 2048, ['x.wav']),
      derived: section(3, 3 * 1024 * 1024, ['d1', 'd2', 'd3']),
      artwork_rows: section(0, 0, []),
      artwork_dirs: section(1, 10, ['thumbs/ab']),
    })
    expect(rows.map((r) => r.key)).toEqual(['tracks', 'albums', 'archived', 'derived', 'artwork_rows', 'artwork_dirs'])
    expect(rows[0]).toEqual({ key: 'tracks', label: '欠落トラック（行の削除）', count: 2, bytes: null, sample: ['a', 'b'] })
    expect(rows[2]).toEqual({ key: 'archived', label: '退避ファイル（期限切れ）', count: 1, bytes: 2048, sample: ['x.wav'] })
    expect(rows[3]!.bytes).toBe(3 * 1024 * 1024)
  })
})

describe('archiveStateLabel', () => {
  it('状態を日本語にする', () => {
    expect(archiveStateLabel('held')).toBe('保持中')
    expect(archiveStateLabel('restored')).toBe('復元済み')
    expect(archiveStateLabel('deleted')).toBe('削除済み')
  })
})
