import { describe, expect, it } from 'vitest'
import { driveStateLabel, newDiscToc, type DriveStatus } from './cdDrive'

function st(state: DriveStatus['state'], toc: string | null = null, error: string | null = null): DriveStatus {
  return { state, toc, error, checked_at: 1_700_000_000 }
}

describe('driveStateLabel', () => {
  it('状態ごとの日本語', () => {
    expect(driveStateLabel(st('unknown'))).toBe('ドライブを確認中…')
    expect(driveStateLabel(st('no_drive', null, 'open: No such file or directory'))).toBe(
      'ドライブが無い（open: No such file or directory）',
    )
    expect(driveStateLabel(st('no_disc'))).toBe('ディスクなし')
    expect(driveStateLabel(st('tray_open'))).toBe('トレイが開いている')
    expect(driveStateLabel(st('not_ready'))).toBe('ドライブの準備中…')
    expect(driveStateLabel(st('disc_ok', '0:20144:40290'))).toBe('ディスクあり（2 トラック）')
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
