// スマホ版が使う API の呼び出し。本体は既存の API（SPEC §9）を呼ぶだけ。
// 後続のタスクで関数を足していく

import { ApiError, apiFetch, apiPost } from '../api/client'
import type { AlbumRow, TrackPage, TrackWithDetail } from '../api/types'
import { tracksUrl, type Filter, type Sort } from '../lib/filter'

/** ログイン済みか。未ログインなら ApiError(401) で落ちる */
export async function getSession(): Promise<void> {
  await apiFetch<unknown>('/api/auth/session')
}

export async function login(password: string): Promise<void> {
  await apiPost<unknown>('/api/auth/login', { password })
}

export function listTracks(filter: Filter, sort: Sort, cursor?: string | null): Promise<TrackPage> {
  return apiFetch<TrackPage>(tracksUrl({ filter, sort, cursor }))
}

export function getTrack(id: number): Promise<TrackWithDetail> {
  return apiFetch<TrackWithDetail>(`/api/tracks/${id}`)
}

export function listAlbums(): Promise<AlbumRow[]> {
  return apiFetch<AlbumRow[]>('/api/albums')
}

/** Inbox の件数。Inbox が設定されていない（503）なら null */
export async function inboxSummary(): Promise<{ pending: number; failed: number } | null> {
  try {
    return await apiFetch<{ pending: number; failed: number }>('/api/inbox/summary')
  } catch (e) {
    if (e instanceof ApiError && e.status === 503) return null
    throw e
  }
}
