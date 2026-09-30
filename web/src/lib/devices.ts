// 端末タブの純粋ロジック（P5-2、D-95）。件数・差分表の並び・文言

import { ApiError } from '../api/client'
import type { AdbVolume, Device, DeviceCounts, DeviceDiff, DiffItem, DiffOp, JobEvent, JobState } from '../api/types'
import { formatDateTime } from './history'

/** 未反映の件数（ナビのバッジ・一覧）。待ちはハッシュ計算などを待っているだけなので数えない */
export function unsyncedCount(c: DeviceCounts): number {
  return c.add + c.update + c.move + c.delete + c.error
}

/** 全端末の未反映の合計（同じ曲が 2 端末で未反映なら 2） */
export function totalBadge(devices: readonly Device[]): number {
  return devices.reduce((n, d) => n + unsyncedCount(d.counts), 0)
}

export const OP_LABELS: Record<DiffOp, string> = {
  add: '追加',
  update: '更新',
  move: '移動',
  update_move: '更新 + 移動',
  delete: '削除',
  waiting: '待ち',
  error: 'エラー',
}

/** 差分表の並び。手当てが要るもの（エラー）と端末から消えるもの（削除）を先に出す */
const ORDER: DiffOp[] = ['error', 'delete', 'update_move', 'move', 'update', 'add', 'waiting']

export function sortDiffItems(items: readonly DiffItem[]): DiffItem[] {
  return [...items].sort(
    (a, b) => ORDER.indexOf(a.op) - ORDER.indexOf(b.op) || (a.title ?? '').localeCompare(b.title ?? '', 'ja'),
  )
}

/** 取った差分（どの端末のものかと組）のうち、いま選んでいる端末のものだけを返す。
 * 端末を切り替えた直後に前の端末の差分を出さない */
export function diffFor(held: { id: number; diff: DeviceDiff } | null, selectedId: number | null): DeviceDiff | null {
  return held != null && held.id === selectedId ? held.diff : null
}

/** 作った端末を一覧に足す（一覧の取り直しを待たずに選べるように）。既にあればそのまま */
export function withDevice(items: readonly Device[] | null, d: Device): Device[] {
  if (items == null) return [d]
  return items.some((x) => x.id === d.id) ? [...items] : [...items, d]
}

/** 同期で送る件数（待ち・エラー以外）と送る量 */
export function syncSummary(d: DeviceDiff): { count: number; bytes: number } {
  return {
    count: d.items.filter((i) => i.op !== 'waiting' && i.op !== 'error').length,
    bytes: d.estimate.transfer_bytes,
  }
}

/** スマートプレイリストの評価時刻の説明。評価待ち（ライブラリの変更をまだ反映していない）を優先する */
export function describeEvaluation(e: DeviceDiff['evaluations'][number], _now: number): string {
  if (e.pending) return `${e.name}: 評価待ち（ライブラリの変更を反映する前の結果）`
  if (e.evaluated_at == null) return `${e.name}: 未評価`
  return `${e.name}: ${formatDateTime(e.evaluated_at)} に評価`
}

const OPEN_PLAN_TITLE = '前回の同期が途中です。続きを実行するか破棄してください'
export const ADB_DISABLED_MESSAGE = 'Android の同期が無効です（設定の [devices].adb_server）'

/** Android の接続状態の説明（問題が無ければ null） */
export function connectionNote(d: Device): string | null {
  if (d.transport !== 'adb' || d.connected) return null
  switch (d.adb_state) {
    case 'unauthorized':
      return '端末で USB デバッグを許可してください'
    case 'offline':
    case 'authorizing':
      return '接続中（端末の応答待ち）'
    default:
      return '未接続（USB でつなぐと差分を取り直します）'
  }
}

export function volumeLabel(v: AdbVolume): string {
  return v.volume === 'emulated' ? '内部共有ストレージ' : `SD カード（${v.volume}）`
}

/** 保存先は空か存在しないときだけ選べる（仕様 ⑤「登録」4） */
export function volumeUsable(v: AdbVolume): boolean {
  return v.state !== 'nonempty'
}

/** 待ちの同期の説明。未接続なら接続を待っている（つながっていれば順番待ち） */
export function queuedSyncText(d: Device): string {
  return d.connected === false ? '端末の接続を待っています' : '同期の待ち'
}

export function syncButton(d: Device, diff: DeviceDiff): { enabled: boolean; title: string | null } {
  if (d.sync_job != null) {
    return { enabled: false, title: d.sync_job.state === 'running' ? '同期中' : queuedSyncText(d) }
  }
  if (d.plan_open) return { enabled: false, title: OPEN_PLAN_TITLE }
  const tracks = diff.items.filter((i) => i.op !== 'waiting' && i.op !== 'error').length
  const lists = diff.playlists.filter((p) => p.op !== 'error').length
  if (tracks + lists === 0) return { enabled: false, title: '差分がありません' }
  return { enabled: true, title: null }
}

export const FORCE_ABANDON_CONFIRM =
  '端末がつながっていない状態で途中の計画を破棄します。端末に残った途中の移動は、次につないだときの回復で完遂または取り消しになります。よろしいですか？'

/** 途中の計画の「強制破棄」（端末につながずに閉じる。D-98）を出すか。端末が未接続か、
 * 通常の破棄が not_connected で断られたとき。同期が実行中なら出さない（サーバも 409 busy） */
export function offerForceAbandon(d: Device, lastErrorCode: string | null): boolean {
  if (d.transport !== 'adb' || !d.plan_open || d.sync_job?.state === 'running') return false
  return d.connected === false || lastErrorCode === 'not_connected'
}

/** ジョブのイベントが状態の変化か（進捗だけのイベントは false）。`seen` はジョブごとの直前の状態で、
 * 終わりの状態は覚えない（大きくならない。再試行で queued に戻っても変化として拾う） */
export function jobStateChanged(seen: Map<number, JobState>, e: Pick<JobEvent, 'id' | 'state'>): boolean {
  if (e.state === 'done' || e.state === 'failed' || e.state === 'cancelled') {
    seen.delete(e.id)
    return true
  }
  const changed = seen.get(e.id) !== e.state
  seen.set(e.id, e.state)
  return changed
}

/** 端末 API のエラーを日本語にする */
export function deviceMessage(e: unknown): string {
  if (e instanceof ApiError) {
    switch (e.code) {
      case 'open_plan':
        return '同期が途中か実行中なので変更できません（完了させるか、計画を破棄してから）'
      case 'cycle':
        return 'このスマートプレイリストは端末の状態（on_device / device_pending）を使っているので、端末の選曲に載せられません'
      case 'duplicate':
        return '同じ名前の端末があります'
      case 'not_found':
        return '端末が見つかりません（削除された可能性があります）'
      case 'not_connected':
        return '端末がつながっていません。USB でつないで、端末で USB デバッグを許可してください'
      case 'not_empty':
        return '保存先が空ではありません。空のフォルダか、まだ無い場所を選んでください'
      case 'serial_registered':
        return 'この端末は登録済みです'
      case 'plan_changed':
        return '差分が変わりました。確認し直してから同期してください'
      case 'open_plan_exists':
        return OPEN_PLAN_TITLE
      case 'pending_reevaluation':
        return 'スマートプレイリストの評価待ちです。評価が終わってから同期してください'
      case 'busy':
        return '端末を別の処理が使っています。しばらくしてからやり直してください'
      case 'no_open_plan':
        return '途中の計画はありません'
      case 'plan_unreadable':
        return '途中の計画を読めません。破棄してください'
      case 'adb_disabled':
        return ADB_DISABLED_MESSAGE
    }
    return e.message
  }
  return e instanceof Error ? e.message : String(e)
}
