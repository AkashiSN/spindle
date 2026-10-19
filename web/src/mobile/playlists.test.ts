import { describe, expect, it } from 'vitest'
import { ApiError } from '../api/client'
import type { Playlist } from '../api/types'
import { deleteErrorMessage, moveTarget, nameErrorMessage, playlistSubtitle } from './playlists'

describe('moveTarget', () => {
  it('moveTarget は前へも後ろへも正しい before を返す', () => {
    const ids = [10, 20, 30, 40]
    expect(moveTarget(ids, 3, 0)).toEqual({ trackId: 40, before: 10 })
    expect(moveTarget(ids, 0, 2)).toEqual({ trackId: 10, before: 40 })
    expect(moveTarget(ids, 1, 3)).toEqual({ trackId: 20, before: null })
  })
  it('1 つ隣へ動かす', () => {
    const ids = [10, 20, 30]
    expect(moveTarget(ids, 0, 1)).toEqual({ trackId: 10, before: 30 })
    expect(moveTarget(ids, 2, 1)).toEqual({ trackId: 30, before: 20 })
  })
  it('範囲外の to は端に寄せる', () => {
    const ids = [10, 20, 30]
    expect(moveTarget(ids, 0, 99)).toEqual({ trackId: 10, before: null })
    expect(moveTarget(ids, 2, -5)).toEqual({ trackId: 30, before: 10 })
  })
})

describe('playlistSubtitle', () => {
  it('playlistSubtitle', () => {
    expect(playlistSubtitle({ kind: 'manual', track_count: 128, duration_ms: 31_320_000 } as Playlist)).toBe(
      '普通 · 128 曲 · 8 時間 42 分',
    )
  })
  it('スマート、1 時間未満、桁区切り', () => {
    expect(playlistSubtitle({ kind: 'smart', track_count: 86, duration_ms: 1_500_000 } as Playlist)).toBe(
      'スマート · 86 曲 · 25 分',
    )
    expect(playlistSubtitle({ kind: 'manual', track_count: 1234, duration_ms: 3_600_000 } as Playlist)).toBe(
      '普通 · 1,234 曲 · 1 時間 0 分',
    )
  })
  it('空なら長さを出さない', () => {
    expect(playlistSubtitle({ kind: 'manual', track_count: 0, duration_ms: 0 } as Playlist)).toBe('普通 · 0 曲')
  })
})

describe('deleteErrorMessage', () => {
  it('deleteErrorMessage は open_plan を言い換える', () => {
    expect(deleteErrorMessage(new ApiError(409, 'open_plan'))).toBe(
      '端末への同期が残っているので削除できません（PC の端末タブで確認）',
    )
  })
  it('それ以外はコード付きの汎用文言', () => {
    expect(deleteErrorMessage(new ApiError(404, 'not_found'))).toBe('削除できませんでした（not_found）')
    expect(deleteErrorMessage(new Error('x'))).toBe('削除できませんでした')
  })
})

describe('nameErrorMessage', () => {
  it('重複と不正な名前を言い換える', () => {
    expect(nameErrorMessage(new ApiError(409, 'duplicate'))).toBe('同じ名前のプレイリストがあります')
    expect(nameErrorMessage(new ApiError(400, 'bad_request', 'name に `/` は使えない'))).toBe(
      '名前が不正です（name に `/` は使えない）',
    )
    expect(nameErrorMessage(new ApiError(500, 'internal'))).toBe('保存できませんでした（internal）')
    expect(nameErrorMessage(new Error('x'))).toBe('保存できませんでした')
  })
})
