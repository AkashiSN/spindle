// 操作タブ（D-58）の純粋ロジック: リネーム / 正規化の preview の集計、投入系の結果メッセージ、
// 409 のコードの日本語化。API の呼び出しは hooks/useOperations

import { formatCount } from './format'

/** `POST /api/rename/preview` / `/api/normalize/preview` の共通部分 */
export type PathPreviewCounts = {
  count: number
  changed: number
  unchanged: number
  conflict: number
  pending_excluded: number
}

export type PathPreviewItem = {
  id: number
  old: string
  /** 宛先。衝突・生成不能なら null で reason に理由 */
  new: string | null
  reason: string | null
  /** 正規化のみ */
  codec?: string
}

export type PathPreview = PathPreviewCounts & { selection_token: string; items: PathPreviewItem[] }

export type PathApplyResponse = { batch_id: number; affected: number; conflict: number }

export type RgStartResponse = { albums: number; tracks: number; duplicates: number; job_ids: number[] }
export type FlaccheckStartResponse = { tracks: number; skipped: number; duplicates: number; job_ids: number[] }
export type RgWriteResponse = {
  batch_id: number | null
  affected: number
  unchanged: number
  unscanned: number
  missing: number
  pending_excluded: number
}

export function pathPreviewSummary(c: PathPreviewCounts): string {
  const parts = [`変更 ${formatCount(c.changed)}`]
  if (c.unchanged > 0) parts.push(`変更なし ${formatCount(c.unchanged)}`)
  if (c.conflict > 0) parts.push(`衝突 ${formatCount(c.conflict)}`)
  if (c.pending_excluded > 0) parts.push(`反映待ちで除外 ${formatCount(c.pending_excluded)}`)
  return parts.join(' / ')
}

export function rgStartedMessage(r: RgStartResponse): string {
  const parts: string[] = []
  if (r.albums > 0) parts.push(`アルバム ${formatCount(r.albums)}`)
  if (r.tracks > 0) parts.push(`単独トラック ${formatCount(r.tracks)}`)
  const dup = r.duplicates > 0 ? `（既に投入済み ${formatCount(r.duplicates)}）` : ''
  return `ReplayGain 解析を投入した: ${parts.join(' / ') || '0'}${dup}`
}

export function flaccheckStartedMessage(r: FlaccheckStartResponse): string {
  const notes: string[] = []
  if (r.skipped > 0) notes.push(`FLAC でない・欠落で対象外 ${formatCount(r.skipped)}`)
  if (r.duplicates > 0) notes.push(`既に投入済み ${formatCount(r.duplicates)}`)
  return `FLAC 検査を投入した: ${formatCount(r.tracks)} 件${notes.length > 0 ? `（${notes.join(' / ')}）` : ''}`
}

export function rgWrittenMessage(r: RgWriteResponse): string {
  const rest: string[] = []
  if (r.unchanged > 0) rest.push(`既に一致 ${formatCount(r.unchanged)}`)
  if (r.unscanned > 0) rest.push(`未解析 ${formatCount(r.unscanned)}`)
  if (r.missing > 0) rest.push(`欠落 ${formatCount(r.missing)}`)
  if (r.pending_excluded > 0) rest.push(`反映待ちで除外 ${formatCount(r.pending_excluded)}`)
  const tail = rest.join(' / ')
  if (r.batch_id == null) return `書く行は無い${tail ? `。${tail}` : ''}`
  return `ReplayGain をタグに書く: ${formatCount(r.affected)} 件（バッチ #${r.batch_id}）${tail ? `。${tail}` : ''}`
}

export type Md5FillResponse = { batch_id: number; affected: number; skipped: number; pending_excluded: number }

export function md5FillMessage(r: Md5FillResponse): string {
  const rest: string[] = []
  if (r.skipped > 0) rest.push(`対象外 ${formatCount(r.skipped)}`)
  if (r.pending_excluded > 0) rest.push(`反映待ちで除外 ${formatCount(r.pending_excluded)}`)
  const tail = rest.join(' / ')
  return `MD5 の補填を投入した: ${formatCount(r.affected)} 件（バッチ #${r.batch_id}）${tail ? `。${tail}` : ''}`
}

const KNOWN: Record<string, string> = {
  no_changes: '対象がありません',
  preview_stale: 'プレビューが古くなりました。もう一度プレビューしてください',
  normalize_disabled: '正規化は設定で無効です（[normalize].wav_to_flac）',
  rg_write_disabled: 'ReplayGain のタグ書き込みは設定で無効です（[replaygain].write_tags）',
  md5_fill_disabled: 'MD5 の補填は設定で無効です（[normalize].flac_fix_missing_md5）',
  editor_unavailable: '編集機能が使えません（読み取り専用で起動している）',
}

/** エラー応答の本文を 1 行に。既知のコードは日本語、それ以外は message かコード + HTTP 状態 */
export function operationErrorMessage(status: number, body: unknown): string {
  const b = (body ?? {}) as { error?: string; message?: string }
  if (b.error && KNOWN[b.error]) return KNOWN[b.error]!
  if (b.message) return b.message
  return `${b.error ?? 'http_error'} (HTTP ${status})`
}
