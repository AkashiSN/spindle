// Mac の同期の進捗（端末タブ）の書式と判定。表示の経過時間はサーバが計算した age を受信時刻に直して持つ

import type { DeviceProgressEvent, SyncProgress } from '../api/types'
import { formatBytes } from './settings'

/** これを超えて進捗が来なければ「応答なし」と見なす（秒） */
export const STALE_SECS = 30

/** 進み具合 0..1。バイトの総量があればバイト、無ければ件数 */
export function progressRatio(p: SyncProgress): number {
  const r = p.bytes_total > 0 ? p.bytes_done / p.bytes_total : p.total > 0 ? p.done / p.total : 0
  return Math.min(1, Math.max(0, r))
}

/** 「45 秒前」「1 分前」「1 時間前」 */
export function agoText(secs: number): string {
  const s = Math.max(0, Math.floor(secs))
  if (s < 60) return `${s} 秒前`
  if (s < 3600) return `${Math.floor(s / 60)} 分前`
  return `${Math.floor(s / 3600)} 時間前`
}

function remaining(secs: number): string {
  const s = Math.max(0, Math.round(secs))
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`
}

/** 1 行の要約。`ageSecs` は最後の進捗からの経過秒 */
export function progressSummary(p: SyncProgress, ageSecs: number): string {
  const parts = [`${p.done} / ${p.total} 件`]
  const hasBytes = p.bytes_total > 0
  if (hasBytes) parts.push(`${formatBytes(p.bytes_done)} / ${formatBytes(p.bytes_total)}`)
  if (ageSecs > STALE_SECS) {
    parts.push(`応答なし（最後の進捗: ${agoText(ageSecs)}）`)
  } else if (hasBytes && p.rate > 0) {
    parts.push(`${formatBytes(p.rate)}/s`)
    parts.push(`残り ${remaining((p.bytes_total - p.bytes_done) / p.rate)}`)
  }
  return parts.join(' · ')
}

export function phaseLabel(phase: SyncProgress['phase']): string {
  switch (phase) {
    case 'delete':
      return '削除中'
    case 'move':
      return '移動中'
    case 'fetch':
      return '受信中'
    case 'add':
      return '追加中'
    case 'playlist':
      return 'プレイリスト'
  }
}

/** 受信時刻（ms）。サーバの age から逆算する */
export function receivedAtOf(p: Pick<SyncProgress, 'age'>, nowMs: number): number {
  return nowMs - p.age * 1000
}

type Progressive = { id: number; sync_progress?: SyncProgress | null }

/**
 * 一覧の応答に SSE の進捗を重ねる。要求を出した後に device_progress が届いた端末は、応答より
 * イベントの方が新しいので現在の sync_progress（null 含む）を残す。それ以外は応答の値を使い、
 * receivedAt を付ける。events は端末 ID → 最後のイベントの受信時刻（ms）
 */
export function mergeListProgress<T extends Progressive>(
  fresh: readonly T[],
  current: readonly Progressive[] | null,
  events: ReadonlyMap<number, number>,
  requestStartMs: number,
  nowMs: number,
): T[] {
  return fresh.map((d) => {
    const at = events.get(d.id)
    if (at != null && at > requestStartMs) {
      const cur = current?.find((x) => x.id === d.id)
      if (cur) return { ...d, sync_progress: cur.sync_progress ?? null }
    }
    const p = d.sync_progress
    return p ? { ...d, sync_progress: { ...p, receivedAt: receivedAtOf(p, nowMs) } } : d
  })
}

type ProgressTarget = Progressive & { plan_open: boolean }

/**
 * SSE の device_progress を一覧へ当てる（一覧に無い端末は無視）。進捗は open な計画にしか来ないので、
 * 非 null なら plan_open も true にする（計画の確定はイベントを流さず、一覧の取り直しまで古いままのため）。
 * null（計画が閉じた）のときは plan_open に触らない。閉じたかどうかは一覧の取り直しで知る
 */
export function applyProgressEvent<T extends ProgressTarget>(items: readonly T[], e: DeviceProgressEvent, nowMs: number): T[] {
  return items.map((d) =>
    d.id !== e.device_id
      ? d
      : e.progress
        ? { ...d, plan_open: true, sync_progress: { ...e.progress, receivedAt: receivedAtOf(e.progress, nowMs) } }
        : { ...d, sync_progress: null },
  )
}
