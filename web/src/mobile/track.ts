// スマホ版の曲のシート・試し聴き・タグの修正の純粋ロジック。
// - 試し聴きは原本を流さない。可逆は Opus（鳴らない端末は AAC）に変換したもの、非可逆は原本（D-8）
// - タグの修正は 1 曲・1 フィールドの set だけ（追加・削除はしない）

import type { TrackDetail, TrackRow } from '../api/types'
import type { Source, Support } from '../lib/playback'
import { splitValues } from '../lib/properties'
import { normKey, type OpRequest } from '../lib/tagops'

export function mobileSource(track: Pick<TrackRow, 'id' | 'lossless'>, support: Support): Source {
  const base = `/api/stream/${track.id}`
  if (!track.lossless) return { url: base, transcode: false }
  return { url: `${base}?transcode=${support.opus ? 'opus' : 'aac'}`, transcode: true }
}

/** タグを編集できない理由。編集できるなら null */
export function editBlockReason(
  row: Pick<TrackRow, 'pending_batch_id' | 'conflict_batch_id' | 'missing_since'>,
): string | null {
  if (row.pending_batch_id != null) return '反映待ちの間は編集できません'
  if (row.conflict_batch_id != null) return '競合を PC で解決してから編集してください'
  if (row.missing_since != null) return 'ファイルが見つかりません'
  return null
}

/** 1 フィールドの修正の ops。多値は PC のプロパティ欄と同じく `;` 区切り */
export function tagEditOps(key: string, text: string): OpRequest[] {
  return [{ op: 'set', key: normKey(key), value: splitValues(text) }]
}

/** 入力値の問題。空（フィールドの削除になる）は保存させない。問題が無ければ null */
export function tagEditProblem(text: string): string | null {
  if (splitValues(text).length === 0) return '空にはできません。フィールドの削除は PC で行ってください'
  return null
}

/** history.state に載せるシートの状態。`edit` はタグの修正シートを開いているキー */
export type SheetState = { trackId: number; editKey: string | null }

export function sheetFromHistory(state: unknown): SheetState | null {
  if (state == null || typeof state !== 'object') return null
  const s = state as { sheet?: unknown; edit?: unknown }
  if (typeof s.sheet !== 'number' || !Number.isSafeInteger(s.sheet) || s.sheet <= 0) return null
  return { trackId: s.sheet, editKey: typeof s.edit === 'string' && s.edit !== '' ? s.edit : null }
}

/** ビット深度 / 周波数のバッジ（`24/96kHz`）。どちらも不明なら null */
export function audioSpec(d: Pick<TrackDetail, 'bit_depth' | 'sample_rate'>): string | null {
  if (d.sample_rate == null) return d.bit_depth != null ? `${d.bit_depth}bit` : null
  const khz = `${Number((d.sample_rate / 1000).toFixed(1))}kHz`
  return d.bit_depth != null ? `${d.bit_depth}/${khz}` : khz
}

/** ファイルサイズ（1024 単位） */
export function formatSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  if (bytes < 1024 ** 2) return `${(bytes / 1024).toFixed(1)} KB`
  if (bytes < 1024 ** 3) return `${(bytes / 1024 ** 2).toFixed(1)} MB`
  return `${(bytes / 1024 ** 3).toFixed(2)} GB`
}
