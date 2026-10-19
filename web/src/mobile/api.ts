// スマホ版が使う API の呼び出し。本体は既存の API（SPEC §9）を呼ぶだけ。
// 後続のタスクで関数を足していく

import { ApiError, apiFetch, apiPatch, apiPost, parseErrorBody } from '../api/client'
import type {
  AlbumRow,
  AppendResponse,
  ApplyResponse,
  Job,
  Playlist,
  PlaylistList,
  PreviewResponse,
  RulePreview,
  Subscription,
  SubscriptionList,
  TrackPage,
  TrackWithDetail,
} from '../api/types'
import { tracksUrl, type Filter, type Sort } from '../lib/filter'
import { tagEditOps } from './track'

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

/** `GET /api/albums` は `{ items }` で返る */
export async function listAlbums(): Promise<AlbumRow[]> {
  const r = await apiFetch<{ items: AlbumRow[] }>('/api/albums')
  return r.items
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

/**
 * 1 曲の 1 フィールドを書き換える。PC と同じ preview → apply（edit_batches に載り、PC の「履歴」から
 * 巻き戻せる）。成功（変化なしを含む）は null、失敗は文言
 */
export async function editTag(trackId: number, key: string, text: string): Promise<string | null> {
  const ops = tagEditOps(key, text)
  try {
    const pv = await apiPost<PreviewResponse>('/api/tracks/batch/preview', { selection: { ids: [trackId] }, ops })
    if (pv.pending_excluded > 0) return '反映待ちの間は編集できません'
    if (pv.changed === 0) return null
    const r = await parseErrorBody<ApplyResponse>('/api/tracks/batch', {
      method: 'PATCH',
      body: JSON.stringify({ selection_token: pv.selection_token, ops }),
    })
    if (r.ok) return null
    const body = r.body as { error?: string; message?: string } | null
    if (body?.error === 'pending') return '反映待ちの行は編集できません'
    return body?.message ?? `${body?.error ?? 'http_error'} (HTTP ${r.status})`
  } catch (e) {
    return e instanceof Error ? e.message : String(e)
  }
}

// ---------------------------------------------------------------- プレイリスト

export async function listPlaylists(): Promise<Playlist[]> {
  const r = await apiFetch<PlaylistList>('/api/playlists')
  return r.items
}

/** 201 で作ったプレイリストを返す。rule があればスマート */
export function createPlaylist(body: { name: string; rule?: string }): Promise<Playlist> {
  return apiPost<Playlist>('/api/playlists', body)
}

/** 変更後のプレイリストを返す */
export function updatePlaylist(id: number, body: { name?: string; rule?: string }): Promise<Playlist> {
  return apiPatch<Playlist>(`/api/playlists/${id}`, body)
}

/** 204。印の付いた端末に作業が残っていれば 409 open_plan の ApiError のまま投げる */
export async function deletePlaylist(id: number): Promise<void> {
  await apiFetch<unknown>(`/api/playlists/${id}`, { method: 'DELETE' })
}

/** 末尾に足す（送った順）。既に入っている曲は skipped に数える */
export function addToPlaylist(id: number, trackIds: number[]): Promise<AppendResponse> {
  return apiPost<AppendResponse>(`/api/playlists/${id}/items`, { selection: { ids: trackIds } })
}

/** 応答は { removed } だが使わない */
export async function removeFromPlaylist(id: number, trackIds: number[]): Promise<void> {
  await apiFetch<unknown>(`/api/playlists/${id}/items`, {
    method: 'DELETE',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ track_ids: trackIds }),
  })
}

/** before の前へ動かす（null は末尾）。204 */
export async function moveInPlaylist(id: number, trackIds: number[], before: number | null): Promise<void> {
  await apiPost<unknown>(`/api/playlists/${id}/items/move`, { track_ids: trackIds, before })
}

export function previewRule(rule: string, signal?: AbortSignal): Promise<RulePreview> {
  return apiFetch<RulePreview>('/api/playlists/preview', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ rule }),
    signal,
  })
}

// ---------------------------------------------------------------- 購読

/** 直近の同期の進み具合（`GET /api/ytmusic/subscriptions/:id/progress`） */
export type SubscriptionProgress = { sync: Job | null; items: Job[] }

/** ytmusic が無効なら 404 の ApiError のまま投げる */
export async function listSubscriptions(): Promise<Subscription[]> {
  const r = await apiFetch<SubscriptionList>('/api/ytmusic/subscriptions')
  return r.items
}

/** 202 → started、409 duplicate（同期が既に待ち・走行中）→ already。それ以外は ApiError */
export async function syncSubscription(id: number): Promise<'started' | 'already'> {
  try {
    await apiPost<unknown>(`/api/ytmusic/subscriptions/${id}/sync`, {})
    return 'started'
  } catch (e) {
    if (e instanceof ApiError && e.status === 409 && e.code === 'duplicate') return 'already'
    throw e
  }
}

export function getSubscriptionProgress(id: number): Promise<SubscriptionProgress> {
  return apiFetch<SubscriptionProgress>(`/api/ytmusic/subscriptions/${id}/progress`)
}
