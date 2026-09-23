import { describe, expect, it } from 'vitest'
import { audioTocSummary, driveIdsFor, driveInfoLabel, driveStateLabel, newDiscToc, type DriveStatus } from './cdDrive'

function st(state: DriveStatus['state'], toc: string | null = null, error: string | null = null): DriveStatus {
  return { state, toc, tracks: [], isrcs: [], mcn: null, error, checked_at: 1_700_000_000 }
}

describe('driveIdsFor', () => {
  const five: DriveStatus = { ...st('disc_ok', '0:20144:40290'), isrcs: ['JPQ402600330', null], mcn: '4582515778491' }
  it('欄の TOC がドライブの盤と同じときだけ ISRC / MCN を添える', () => {
    expect(driveIdsFor('0:20144:40290', five)).toEqual({ isrcs: ['JPQ402600330', null], mcn: '4582515778491' })
    // 正規化してから比べる（前後の空白、cdrecord -toc の出力）
    expect(driveIdsFor('  0:20144:40290\n', five)).toEqual({ isrcs: ['JPQ402600330', null], mcn: '4582515778491' })
  })
  it('別の盤の TOC（貼り付け・編集後）やドライブ無しでは空（codex 指摘: 盤 A の ISRC を盤 B に混ぜない）', () => {
    expect(driveIdsFor('0:20000:40000', five)).toEqual({ isrcs: [], mcn: null })
    expect(driveIdsFor('0:20144:40290', st('no_disc'))).toEqual({ isrcs: [], mcn: null })
    expect(driveIdsFor('0:20144:40290', null)).toEqual({ isrcs: [], mcn: null })
    expect(driveIdsFor('', five)).toEqual({ isrcs: [], mcn: null })
  })
})

describe('audioTocSummary', () => {
  it('音声トラックだけ数え、総時間は音声区間の長さ', () => {
    // 2 トラック、リードアウト 40290 セクタ = 8:57
    expect(audioTocSummary('0:20144:40290')).toEqual({ tracks: 2, durationMs: (40290 * 1000) / 75 })
  })
  it('Enhanced CD はデータトラックを数えず、音声の終端はデータ開始 − 11400（D-64 / SPEC §7.2）', () => {
    // 音声 2 本 + データ 1 本。音声の終端は 125824 − 11400 = 114424 セクタ
    expect(audioTocSummary('0:13959:-125824:188333')).toEqual({
      tracks: 2,
      durationMs: (114424 * 1000) / 75,
    })
  })
  it('先頭にデータがある Mixed Mode は音声の開始から数える', () => {
    // データ 1 本 + 音声 2 本（データが先頭）。音声は 2 本、区間は 30000 → 90000
    expect(audioTocSummary('-0:30000:60000:90000')).toEqual({
      tracks: 2,
      durationMs: (60000 * 1000) / 75,
    })
  })
  it('読めない TOC は null', () => {
    expect(audioTocSummary('x:y')).toBeNull()
    expect(audioTocSummary('')).toBeNull()
    expect(audioTocSummary('-0:1000')).toBeNull()
  })
})

describe('driveStateLabel', () => {
  it('状態ごとの日本語', () => {
    expect(driveStateLabel(st('unknown'))).toBe('ドライブを確認中…')
    expect(driveStateLabel(st('no_drive', null, 'open: No such file or directory'))).toBe(
      'ドライブが無い（open: No such file or directory）',
    )
    expect(driveStateLabel(st('no_disc'))).toBe('ディスクなし')
    expect(driveStateLabel(st('tray_open'))).toBe('トレイが開いている')
    expect(driveStateLabel(st('not_ready'))).toBe('ドライブの準備中…')
    // 総時間も出す（40290 − 0 セクタ = 8:57）
    expect(driveStateLabel(st('disc_ok', '0:20144:40290'))).toBe('ディスクあり（2 トラック・8:57）')
    // Enhanced CD は音声トラックだけ数える
    expect(driveStateLabel(st('disc_ok', '0:13959:-125824:188333'))).toBe('ディスクあり（2 トラック・25:26）')
    // 読めない TOC はディスクがあることだけ
    expect(driveStateLabel(st('disc_ok', 'x:y'))).toBe('ディスクあり')
  })
  it('ディスクはあるが TOC を読めていない', () => {
    expect(driveStateLabel(st('disc_ok', null))).toBe('ディスクあり（TOC を読み取り中…）')
    expect(driveStateLabel(st('disc_ok', null, 'READ TOC: Input/output error'))).toBe(
      'ディスクあり（TOC を読めない: READ TOC: Input/output error）',
    )
  })
  it('接続が切れていれば null（表示しない）', () => {
    expect(driveStateLabel(null)).toBe(null)
  })
})

describe('newDiscToc', () => {
  it('前回見た TOC と違う TOC が出たらそれを返す（挿入 = 自動照会の合図）', () => {
    expect(newDiscToc(null, st('disc_ok', '0:20144:40290'))).toBe('0:20144:40290')
    expect(newDiscToc('0:1:2', st('disc_ok', '0:20144:40290'))).toBe('0:20144:40290')
  })
  it('同じディスクの間は null（ポーリングのたびに照会しない）', () => {
    expect(newDiscToc('0:20144:40290', st('disc_ok', '0:20144:40290'))).toBe(null)
  })
  it('抜かれた・TOC が無いときは null', () => {
    expect(newDiscToc('0:20144:40290', st('no_disc'))).toBe(null)
    expect(newDiscToc(null, st('disc_ok', null))).toBe(null)
    expect(newDiscToc(null, null)).toBe(null)
  })
})

describe('driveInfoLabel', () => {
  it('型番の空白を詰め、オフセットの符号と出所を添える', () => {
    expect(driveInfoLabel({ model: 'PIONEER BD-RW   BDR-209M', offset: 667, offset_source: 'table' })).toBe(
      'PIONEER BD-RW BDR-209M、読み取りオフセット +667（AccurateRip のドライブ表）',
    )
    expect(driveInfoLabel({ model: 'X', offset: -30, offset_source: 'learned' })).toBe(
      'X、読み取りオフセット -30（照合で学習済み）',
    )
    expect(driveInfoLabel({ model: 'X', offset: 0, offset_source: 'unknown' })).toContain('0（不明')
  })
})
