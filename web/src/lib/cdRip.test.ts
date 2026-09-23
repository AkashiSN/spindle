import { describe, expect, it } from 'vitest'
import { ripCellLabel, ripErrorMessage, ripProgressFrom, ripStatusLabel, type RipProgress } from './cdRip'

const p = (over: Partial<RipProgress>): RipProgress => ({
  phase: 'read',
  attempt: 1,
  disc_no: 1,
  track_no: 1,
  done: 0,
  total: 100,
  ...over,
})
const nums = [1, 2, 3]
const row = (q: RipProgress) => nums.map((n) => ripCellLabel(n, q, nums))

describe('ripProgressFrom', () => {
  it('サーバの RipProgress を読み、形が違えば null', () => {
    expect(ripProgressFrom({ phase: 'encode', attempt: 1, disc_no: 1, track_no: 2, done: 2, total: 3 })).toEqual({
      phase: 'encode',
      attempt: 1,
      disc_no: 1,
      track_no: 2,
      done: 2,
      total: 3,
    })
    expect(ripProgressFrom({ phase: 'verify', attempt: 2, disc_no: 1, track_no: null, done: 1, total: 9 })?.track_no).toBe(
      null,
    )
    expect(ripProgressFrom(null)).toBe(null)
    expect(ripProgressFrom({ phase: 'scan', attempt: 1, disc_no: 1, done: 0, total: 1 })).toBe(null)
    expect(ripProgressFrom({ phase: 'read', disc_no: 1, done: 0, total: 1 })).toBe(null)
  })
})

describe('ripCellLabel', () => {
  it('読み取り: 読み終えた行・読んでいる行・まだの行', () => {
    expect(row(p({ track_no: 2, done: 50 }))).toEqual(['読んだ', '読み取り中', ''])
    expect(row(p({ track_no: 3, done: 100 }))).toEqual(['読んだ', '読んだ', '読んだ'])
    expect(row(p({ track_no: null }))).toEqual(['', '', ''])
  })
  it('照合・修復はトラックに分かれない', () => {
    expect(row(p({ phase: 'verify', track_no: null }))).toEqual(['照合中', '照合中', '照合中'])
    expect(row(p({ phase: 'repair', track_no: null }))).toEqual(['修復中', '修復中', '修復中'])
  })
  it('エンコード: 終えた行と次の行（track_no は終えたトラック）', () => {
    expect(row(p({ phase: 'encode', track_no: 1, done: 0, total: 3 }))).toEqual(['エンコード中', '', ''])
    expect(row(p({ phase: 'encode', track_no: 1, done: 1, total: 3 }))).toEqual(['FLAC 済', 'エンコード中', ''])
    expect(row(p({ phase: 'encode', track_no: 3, done: 3, total: 3 }))).toEqual(['FLAC 済', 'FLAC 済', 'FLAC 済'])
  })
  it('配置は全部の行', () => {
    expect(row(p({ phase: 'place', track_no: null, done: 1, total: 1 }))).toEqual(['Inbox へ', 'Inbox へ', 'Inbox へ'])
  })
})

describe('ripStatusLabel', () => {
  it('相と割合、吸い直しの回数', () => {
    expect(ripStatusLabel(p({ done: 45 }))).toBe('読み取り 45%')
    expect(ripStatusLabel(p({ phase: 'verify', attempt: 2, done: 80 }))).toBe(
      '照合 80%（2 回目。照合が通らないので吸い直している）',
    )
    expect(ripStatusLabel(p({ phase: 'encode', done: 2, total: 3 }))).toBe('エンコード 66% 2/3 曲')
    expect(ripStatusLabel(p({ phase: 'place', done: 1, total: 1 }))).toBe('Inbox に配置 100%')
  })
})

describe('ripErrorMessage', () => {
  it('API のエラーコードを人向けに', () => {
    expect(ripErrorMessage('duplicate', '')).toContain('別の吸い出し')
    expect(ripErrorMessage('disc_mismatch', '')).toContain('盤が変わった')
    expect(ripErrorMessage('bad_metadata', 'トラック数が違う')).toBe('取り込めない: トラック数が違う')
    expect(ripErrorMessage('other', 'x')).toBe('x')
  })
})
