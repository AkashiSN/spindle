// 設定画面（SPEC §12.6）の純粋ロジック: GC preview の表、バイト数の表記、退避台帳のラベル

export type GcSection = { count: number; bytes: number; sample: string[] }
export type GcPreview = {
  now: number
  cutoff: number
  tracks: GcSection
  albums: GcSection
  archived: GcSection
  derived: GcSection
  artwork_rows: GcSection
  artwork_dirs: GcSection
}

export type ArchiveState = 'held' | 'restored' | 'deleted'
export type ArchiveReason = 'normalize' | 'restore'

export type ArchivedEntry = {
  id: number
  track_id: number | null
  op_id: number | null
  rel_path: string
  source_rel_path: string
  reason: ArchiveReason
  archived_at: number
  eligible_after: number
  state: ArchiveState
  state_at: number | null
  batch_id: number | null
}

export function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`
  const units = ['KB', 'MB', 'GB', 'TB']
  let v = n / 1024
  let i = 0
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024
    i++
  }
  return `${v.toFixed(1)} ${units[i]}`
}

export type GcRow = {
  key: keyof Omit<GcPreview, 'now' | 'cutoff'>
  label: string
  count: number
  /** バイト数を持たない区分（DB の行だけ）は null */
  bytes: number | null
  sample: string[]
}

const GC_LABELS: Array<[GcRow['key'], string, boolean]> = [
  ['tracks', '欠落トラック（行の削除）', false],
  ['albums', '空になったアルバム（行の削除）', false],
  ['archived', '退避ファイル（期限切れ）', true],
  ['derived', 'Derived の孤児', true],
  ['artwork_rows', 'アートワークの行（参照なし）', false],
  ['artwork_dirs', 'サムネイルのディレクトリ（行なし）', true],
]

export function gcPreviewRows(p: GcPreview): GcRow[] {
  return GC_LABELS.map(([key, label, hasBytes]) => ({
    key,
    label,
    count: p[key].count,
    bytes: hasBytes ? p[key].bytes : null,
    sample: p[key].sample,
  }))
}

export function archiveStateLabel(s: ArchiveState): string {
  switch (s) {
    case 'held':
      return '保持中'
    case 'restored':
      return '復元済み'
    case 'deleted':
      return '削除済み'
  }
}

export function archiveReasonLabel(r: ArchiveReason): string {
  return r === 'normalize' ? '正規化で置換' : '巻き戻しで退避'
}
