// プロパティタブ（foobar2000 の Selection Properties の再現。D-58）の純粋ロジック。
// 選択行（TrackRow、一覧に載っている）と `GET /api/tracks/:id` の detail（先頭 DETAIL_LIMIT 件だけ取る）
// から Metadata / Location / General の 3 表を組む。複数選択は共通値、異なれば multiple

import type { Derived, TrackDetail, TrackRow } from '../api/types'
import { HIRES_LABEL, VERIFICATION, hiresMeasurements } from './badges'
import { formatDuration } from './format'
import { keyProblem, normKey } from './tagops'

/** 詳細を取りに行く選択行の上限（それ以上は読み込み済みの行だけで判定） */
export const DETAIL_LIMIT = 50

/** foobar2000 の標準フィールド（この順。空でも出す） */
export const STANDARD_KEYS = [
  'ARTIST',
  'TITLE',
  'ALBUM',
  'DATE',
  'GENRE',
  'COMPOSER',
  'PERFORMER',
  'ALBUMARTIST',
  'TRACKNUMBER',
  'TOTALTRACKS',
  'DISCNUMBER',
  'TOTALDISCS',
  'COMMENT',
] as const

const STANDARD_LABEL: Record<(typeof STANDARD_KEYS)[number], string> = {
  ARTIST: 'Artist Name',
  TITLE: 'Track Title',
  ALBUM: 'Album Title',
  DATE: 'Date',
  GENRE: 'Genre',
  COMPOSER: 'Composer',
  PERFORMER: 'Performer',
  ALBUMARTIST: 'Album Artist',
  TRACKNUMBER: 'Track Number',
  TOTALTRACKS: 'Total Tracks',
  DISCNUMBER: 'Disc Number',
  TOTALDISCS: 'Total Discs',
  COMMENT: 'Comment',
}

export type PropValue = { kind: 'text'; text: string } | { kind: 'multiple' } | { kind: 'empty' }

export type PropRow = {
  /** Metadata ではタグキー（大文字）、他の表では固定の識別子 */
  key: string
  label: string
  value: PropValue
}

const text = (t: string): PropValue => ({ kind: 'text', text: t })
const MULTIPLE: PropValue = { kind: 'multiple' }
const EMPTY: PropValue = { kind: 'empty' }

/** 選択全体で同じ値なら text、全部空なら empty、それ以外（欠けを含む）は multiple */
export function commonValue(values: ReadonlyArray<string | null | undefined>): PropValue {
  if (values.length === 0) return EMPTY
  const first = values[0] ?? ''
  for (const v of values) if ((v ?? '') !== first) return MULTIPLE
  return first === '' ? EMPTY : text(first)
}

/** 多値の表示（foobar と同じ "; " 区切り） */
export function joinValues(values: readonly string[]): string {
  return values.join('; ')
}

/** 編集した文字列を多値に戻す。空は落とす */
export function splitValues(s: string): string[] {
  return s
    .split(';')
    .map((v) => v.trim())
    .filter((v) => v !== '')
}

/**
 * 「フィールドを追加」のキーの検証（P4-3、D-72）。tagops と同じ規則に加えて、表に既にあるキーは
 * その行のダブルクリックで編集するよう案内する。問題が無ければ null
 */
export function newFieldKeyProblem(key: string, existing: readonly string[]): string | null {
  const kp = keyProblem(key)
  if (kp) return kp
  const k = normKey(key)
  if (existing.includes(k)) return `${k} は既にあります。その行をダブルクリックで編集してください`
  return null
}

/** 行の「フィールドを削除」が押せるか（値が無い行は消すものが無い。複数の値なら選択の一部にはある） */
export function canDeleteRow(row: Pick<PropRow, 'value'>): boolean {
  return row.value.kind !== 'empty'
}

export function metadataRows(details: readonly TrackDetail[]): PropRow[] {
  const keys = new Set<string>(STANDARD_KEYS)
  for (const d of details) for (const k of Object.keys(d.tags)) keys.add(k)
  // PICTURE は埋め込み画像のハッシュ（domain::tags）で、表示にも編集にも使えない
  const extra = [...keys].filter((k) => !(STANDARD_KEYS as readonly string[]).includes(k) && k !== 'PICTURE').sort()
  const rowOf = (key: string, label: string): PropRow => ({
    key,
    label,
    value: commonValue(details.map((d) => joinValues(d.tags[key] ?? []))),
  })
  return [...STANDARD_KEYS.map((k) => rowOf(k, STANDARD_LABEL[k])), ...extra.map((k) => rowOf(k, k))]
}

/** epoch 秒 → "YYYY-MM-DD HH:MM"（ローカル時刻） */
export function formatEpoch(epoch: number): string {
  const d = new Date(epoch * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getFullYear()}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`
}

function formatBytes(n: number): string {
  return `${n.toLocaleString('ja-JP')} B`
}

function dirname(path: string): string {
  const i = path.lastIndexOf('/')
  return i < 0 ? '' : path.slice(0, i)
}

function basename(path: string): string {
  return path.slice(path.lastIndexOf('/') + 1)
}

/** 詳細が届いている行の値を集める（届いていない行は数に入れない） */
function withDetail<T>(
  rows: readonly TrackRow[],
  details: ReadonlyMap<number, TrackDetail>,
  pick: (d: TrackDetail) => T,
): T[] {
  const out: T[] = []
  for (const r of rows) {
    const d = details.get(r.id)
    if (d) out.push(pick(d))
  }
  return out
}

/** 合計値。全行の詳細が揃っていなければ「（n / N 件）」を添える */
function total(rows: readonly TrackRow[], values: readonly number[], fmt: (n: number) => string): PropValue {
  if (values.length === 0) return EMPTY
  const sum = values.reduce((a, b) => a + b, 0)
  if (rows.length === 1) return text(fmt(sum))
  const note = values.length === rows.length ? `${rows.length} 件` : `${values.length} / ${rows.length} 件`
  return text(`${fmt(sum)}（${note}）`)
}

export function locationRows(rows: readonly TrackRow[], details: ReadonlyMap<number, TrackDetail>): PropRow[] {
  return [
    { key: 'path', label: 'File path', value: commonValue(rows.map((r) => r.rel_path)) },
    { key: 'folder', label: 'Folder', value: commonValue(rows.map((r) => dirname(r.rel_path))) },
    { key: 'name', label: 'File name', value: commonValue(rows.map((r) => basename(r.rel_path))) },
    {
      key: 'size',
      label: 'File size',
      value: total(
        rows,
        withDetail(rows, details, (d) => d.size),
        formatBytes,
      ),
    },
    {
      key: 'mtime',
      label: 'Last modified',
      value: commonValue(withDetail(rows, details, (d) => formatEpoch(d.mtime))),
    },
    {
      key: 'added',
      label: 'Added',
      value: commonValue(withDetail(rows, details, (d) => formatEpoch(d.added_at))),
    },
    // Derived は系統ごとに 1 行（SPEC §7.6 / §12.2。所在なので Location 側）
    { key: 'derived', label: 'Derived (opus)', value: commonValue(rows.map((r) => derivedLabel(r.derived.opus))) },
    { key: 'derived_aac', label: 'Derived (aac)', value: commonValue(rows.map((r) => derivedLabel(r.derived.aac))) },
  ]
}

function codecLabel(r: TrackRow): string {
  return `${r.codec.toUpperCase()}（${r.lossless ? '可逆' : '非可逆'}）`
}

function rgLabel(r: TrackRow): string {
  if (!r.rg) return ''
  const db = (v: number) => `${v.toFixed(2)} dB`
  const peak = (v: number) => v.toFixed(6)
  let s = `track ${db(r.rg.track_gain)} / peak ${peak(r.rg.track_peak)}`
  if (r.rg.album_gain != null && r.rg.album_peak != null) {
    s += `, album ${db(r.rg.album_gain)} / peak ${peak(r.rg.album_peak)}`
  }
  if (r.rg_written_at == null) s += '（未書き込み）'
  return s
}

function flacCheckLabel(r: TrackRow): string {
  const c = r.flac_check
  if (!c) return ''
  const stale = c.stale ? '（結果が古い）' : ''
  switch (c.status) {
    case 'ok':
      return `OK${stale}`
    case 'md5_missing':
      return `MD5 無し${stale}`
    case 'decode_error':
      return `デコードエラー${stale}${c.error ? `: ${c.error}` : ''}`
  }
}

function hiresCheckLabel(r: TrackRow): string {
  const h = r.hires_check
  if (!h) return ''
  const stale = h.stale ? '（結果が古い）' : ''
  if (h.status === 'decode_error') return `デコードエラー${stale}${h.error ? `: ${h.error}` : ''}`
  const m = hiresMeasurements(h)
  return `${HIRES_LABEL[h.status]}${m ? `（${m}）` : ''}${stale}`
}

function derivedLabel(d: Derived | null): string {
  if (!d) return ''
  return `${d.codec}${d.stale_tags ? '（タグが古い）' : ''}`
}

function stateLabel(r: TrackRow): string {
  const parts: string[] = []
  if (r.pending_batch_id != null) parts.push(`反映待ち #${r.pending_batch_id}`)
  if (r.conflict_batch_id != null) parts.push(`衝突 #${r.conflict_batch_id}`)
  if (r.duplicate_group != null) parts.push('重複')
  if (r.hardlink) parts.push('hardlink')
  if (r.missing_since != null) parts.push('欠落')
  return parts.join(', ')
}

export function generalRows(rows: readonly TrackRow[], details: ReadonlyMap<number, TrackDetail>): PropRow[] {
  const unit = (pick: (d: TrackDetail) => number | null, suffix: string): PropValue =>
    commonValue(withDetail(rows, details, (d) => (pick(d) == null ? '' : `${pick(d)}${suffix}`)))
  return [
    {
      key: 'duration',
      label: 'Duration',
      value: total(
        rows,
        rows.map((r) => r.duration_ms).filter((v): v is number => v != null),
        formatDuration,
      ),
    },
    { key: 'codec', label: 'Codec', value: commonValue(rows.map(codecLabel)) },
    { key: 'sample_rate', label: 'Sample rate', value: unit((d) => d.sample_rate, ' Hz') },
    { key: 'bit_depth', label: 'Bit depth', value: unit((d) => d.bit_depth, ' bit') },
    { key: 'channels', label: 'Channels', value: unit((d) => d.channels, '') },
    { key: 'bitrate', label: 'Bitrate', value: unit((d) => d.bitrate, ' kbps') },
    { key: 'audio_md5', label: 'Audio MD5', value: commonValue(withDetail(rows, details, (d) => d.audio_md5)) },
    {
      key: 'original_codec',
      label: 'Original codec',
      value: commonValue(withDetail(rows, details, (d) => d.original_codec)),
    },
    {
      key: 'verification',
      label: 'Verification',
      value: commonValue(rows.map((r) => (VERIFICATION[r.verification] ?? VERIFICATION.not_attempted).label)),
    },
    { key: 'rg', label: 'ReplayGain', value: commonValue(rows.map(rgLabel)) },
    { key: 'flac_check', label: 'FLAC check', value: commonValue(rows.map(flacCheckLabel)) },
    { key: 'hires_check', label: 'Hi-Res check', value: commonValue(rows.map(hiresCheckLabel)) },
    { key: 'state', label: 'State', value: commonValue(rows.map(stateLabel)) },
  ]
}
