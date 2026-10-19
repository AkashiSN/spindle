// プレイリストのタブの純粋な部品（見出しの文言、並べ替えの before、削除の失敗の文言）

import { ApiError } from '../api/client'
import type { Playlist } from '../api/types'
import { formatCount } from '../lib/format'

/** 「8 時間 42 分」「25 分」（分は四捨五入） */
function hoursMinutes(ms: number): string {
  const total = Math.round(ms / 60_000)
  const h = Math.floor(total / 60)
  const m = total % 60
  return h > 0 ? `${h} 時間 ${m} 分` : `${m} 分`
}

/** 一覧の 2 行目: 「普通 · 128 曲 · 8 時間 42 分」。空なら長さは出さない */
export function playlistSubtitle(p: Pick<Playlist, 'kind' | 'track_count' | 'duration_ms'>): string {
  const parts = [p.kind === 'smart' ? 'スマート' : '普通', `${formatCount(p.track_count)} 曲`]
  if (p.duration_ms > 0) parts.push(hoursMinutes(p.duration_ms))
  return parts.join(' · ')
}

/**
 * 表示順 ids の from 番目を to 番目へ動かすときの `POST .../items/move` の before。
 * 動かした後に to 番目に来るよう、後ろへ動かすときは to の次の行の前に入れる。末尾なら null
 */
export function moveTarget(ids: readonly number[], from: number, to: number): { trackId: number; before: number | null } {
  const trackId = ids[from] as number
  const rest = ids.filter((_, i) => i !== from)
  const at = Math.max(0, Math.min(to, rest.length))
  return { trackId, before: rest[at] ?? null }
}

/** 削除の失敗の文言。印の付いた端末に作業が残っている（409 open_plan）ときは PC の端末タブへ案内する */
export function deleteErrorMessage(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.status === 409 && e.code === 'open_plan') return '端末への同期が残っているので削除できません（PC の端末タブで確認）'
    return `削除できませんでした（${e.code}）`
  }
  return '削除できませんでした'
}

/** 作成・名前の変更の失敗の文言 */
export function nameErrorMessage(e: unknown): string {
  if (e instanceof ApiError) {
    if (e.code === 'duplicate') return '同じ名前のプレイリストがあります'
    if (e.status === 400) return `名前が不正です（${e.message}）`
    return `保存できませんでした（${e.code}）`
  }
  return '保存できませんでした'
}
