// 同期の進捗（端末タブ）の書式と判定。Mac は device_progress、Android は device_sync ジョブの job イベントで届く。
// Mac の表示の経過時間はサーバが計算した age を受信時刻に直して持つ

import type { AdbProgress, DeviceProgressEvent, JobEvent, SyncProgress } from '../api/types'
import { formatBytes } from './settings'

/** これを超えて進捗が来なければ「応答なし」と見なす（秒） */
export const STALE_SECS = 30

/** 進み具合と要約に使う列（Mac と Android で共通） */
export type ProgressCounts = Pick<SyncProgress, 'done' | 'total' | 'bytes_done' | 'bytes_total' | 'rate'>

/** 進み具合 0..1。バイトの総量があればバイト、無ければ件数 */
export function progressRatio(p: ProgressCounts): number {
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

/**
 * 1 行の要約。`ageSecs` は最後の進捗からの経過秒。`stale: false` なら「応答なし」を出さない
 * （Android はジョブの生死をワーカーが管理し、移動のバッチのように進捗の間が空く段がある）
 */
export function progressSummary(p: ProgressCounts, ageSecs: number, opts: { stale?: boolean } = {}): string {
  const parts = [`${p.done} / ${p.total} 件`]
  const hasBytes = p.bytes_total > 0
  if (hasBytes) parts.push(`${formatBytes(p.bytes_done)} / ${formatBytes(p.bytes_total)}`)
  if ((opts.stale ?? true) && ageSecs > STALE_SECS) {
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
    case 'send':
      return '送信中'
    case 'playlist':
      return 'プレイリスト'
  }
}

/** 受信時刻（ms）。サーバの age から逆算する */
export function receivedAtOf(p: Pick<SyncProgress, 'age'>, nowMs: number): number {
  return nowMs - p.age * 1000
}

/** 最後の進捗からの経過秒。受信時刻が無ければサーバの age をそのまま使う */
export function progressAgeSecs(p: SyncProgress & { receivedAt?: number }, nowMs: number): number {
  const received = p.receivedAt ?? receivedAtOf(p, nowMs)
  return Math.max(0, Math.floor((nowMs - received) / 1000))
}

type Progressive = { id: number; sync_progress?: SyncProgress | null }

/**
 * device_progress の到着順の番号。時刻（ms）で比べると、要求と同じ 1 ms に届いたイベントが古い一覧の
 * 応答に負ける。番号はイベントのたびに 1 つ進むので、要求の開始時の `mark()` より後に届いたかを取り違えない
 */
export class ProgressEventSeq {
  private seq = 0
  private last = new Map<number, number>()

  /** イベントが届いた */
  note(deviceId: number): void {
    this.seq += 1
    this.last.set(deviceId, this.seq)
  }

  /** 一覧の要求を出す時点の印 */
  mark(): number {
    return this.seq
  }

  /** `mark` の後にこの端末のイベントが届いたか */
  since(deviceId: number, mark: number): boolean {
    return (this.last.get(deviceId) ?? 0) > mark
  }
}

/**
 * 一覧の応答に SSE の進捗を重ねる。要求を出した後に device_progress が届いた端末は、応答より
 * イベントの方が新しいので現在の sync_progress（null 含む）を残す。それ以外は応答の値を使い、
 * receivedAt を付ける。`newer(id)` はその端末に要求の後のイベントがあったか（`ProgressEventSeq.since`）
 */
export function mergeListProgress<T extends Progressive>(
  fresh: readonly T[],
  current: readonly Progressive[] | null,
  newer: (deviceId: number) => boolean,
  nowMs: number,
): T[] {
  return fresh.map((d) => {
    if (newer(d.id)) {
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
 * null（計画が閉じた）のときは plan_open に触らない。呼び出し側が一覧を取り直して知る（`useDevices`）
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

const ADB_PHASES: readonly AdbProgress['phase'][] = ['delete', 'move', 'send', 'playlist']

/** device_sync ジョブの job イベントから Android の進捗を読む。形が合わなければ null */
export function adbProgressFrom(e: JobEvent): AdbProgress | null {
  const detail = e.detail
  if (detail == null || typeof detail !== 'object') return null
  const d = detail as Record<string, unknown>
  if (d.kind !== 'device_sync') return null
  const num = (v: unknown) => (typeof v === 'number' && Number.isFinite(v) ? v : null)
  if (typeof d.phase !== 'string' || !ADB_PHASES.includes(d.phase as AdbProgress['phase'])) return null
  const bytes_done = num(d.bytes_done)
  const bytes_total = num(d.bytes_total)
  const rate = num(d.rate)
  const done = num(e.done)
  const total = num(e.total)
  if (bytes_done == null || bytes_total == null || rate == null || done == null || total == null) return null
  const current = typeof d.current === 'string' ? d.current : null
  return { done, total, bytes_done, bytes_total, rate, phase: d.phase as AdbProgress['phase'], current }
}

/**
 * ジョブ id → Android の進捗の表を job イベントで更新する。running で進捗が読めれば入れ、終端
 * （running でも queued でもない状態）なら消す。それ以外は同じ表を返す（React の state を無駄に変えない）
 */
export function applyJobProgress(
  map: ReadonlyMap<number, AdbProgress>,
  e: JobEvent,
): ReadonlyMap<number, AdbProgress> {
  if (e.state === 'running') {
    const p = adbProgressFrom(e)
    if (p == null) return map
    const next = new Map(map)
    next.set(e.id, p)
    return next
  }
  if (e.state === 'queued' || !map.has(e.id)) return map
  const next = new Map(map)
  next.delete(e.id)
  return next
}
