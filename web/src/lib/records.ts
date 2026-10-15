// 記録画面（SPEC §12.4、D-106）の純ロジック: タブ、退避ファイルの切り替えと空の文言

import { formatCount } from './format'

export type RecordsTab = 'history' | 'archive' | 'logs'

export const RECORDS_TABS: Array<[RecordsTab, string]> = [
  ['history', '編集履歴'],
  ['archive', '退避ファイル'],
  ['logs', 'ログ'],
]

/** 他の画面から記録画面へ飛ぶときの行き先 */
export type RecordsFocus = { tab: 'history'; batchId?: number } | { tab: 'archive' } | { tab: 'logs'; albumId?: number }

export type ArchiveFilter = 'held' | 'restored' | 'deleted' | 'all'
export type ArchiveCounts = { held: number; restored: number; deleted: number }

export function archiveFilterTabs(c: ArchiveCounts): Array<{ key: ArchiveFilter; label: string; count: number }> {
  return [
    { key: 'held', label: '保持中', count: c.held },
    { key: 'restored', label: '復元済み', count: c.restored },
    { key: 'deleted', label: '削除済み', count: c.deleted },
    { key: 'all', label: 'すべて', count: c.held + c.restored + c.deleted },
  ]
}

export function archiveEmptyText(filter: ArchiveFilter, c: ArchiveCounts): string {
  if (filter !== 'held') return '該当する退避ファイルは無い'
  return c.deleted > 0
    ? `保持中の退避ファイルは無い（削除済み ${formatCount(c.deleted)} 件は GC で実体を消した記録）`
    : '保持中の退避ファイルは無い'
}

export type LogKind = 'all' | 'retro' | 'rip'
export type LogDisc = { disc_no: number | null; method: string; result: string }
export type LogRun = {
  key: string
  album_id: number
  albumartist: string | null
  album: string | null
  source: 'retro' | 'rip'
  job_id: number | null
  verified_at: number
  discs: LogDisc[]
  log: 'latest' | 'superseded' | 'none'
}

const METHOD_LABEL: Record<string, string> = { ctdb: 'CTDB', accuraterip: 'AR' }
const RESULT_LABEL: Record<string, string> = {
  verified: '一致',
  mismatch: '不一致',
  not_found: '登録なし',
  unverifiable: '検証不能',
}

/** 手法 × ディスクの結果を 1 行に。CTDB を先に（サーバも disc, CTDB → AR の順で返す） */
export function logSummary(discs: readonly LogDisc[]): string {
  const order = (m: string) => (m === 'ctdb' ? 0 : 1)
  const byDisc = new Map<number | null, LogDisc[]>()
  for (const d of discs) byDisc.set(d.disc_no, [...(byDisc.get(d.disc_no) ?? []), d])
  const parts = [...byDisc.entries()].map(([disc, ds]) => {
    const text = [...ds]
      .sort((a, b) => order(a.method) - order(b.method))
      .map((d) => `${METHOD_LABEL[d.method] ?? d.method} ${RESULT_LABEL[d.result] ?? d.result}`)
      .join(' / ')
    return byDisc.size > 1 ? `disc ${disc ?? '?'}: ${text}` : text
  })
  return parts.join(' · ')
}

export function logKindLabel(source: 'retro' | 'rip'): string {
  return source === 'retro' ? '照合' : '吸い出し'
}

export function logAvailText(log: LogRun['log']): string | null {
  if (log === 'superseded') return 'このログは後の照合で上書きされた'
  if (log === 'none') return 'ログは残っていない'
  return null
}

/** 行がすべて同じ album ならその id（プロパティの「ログを見る」） */
export function singleAlbumId(rows: ReadonlyArray<{ album_id: number | null }>): number | null {
  const first = rows[0]?.album_id ?? null
  if (first == null) return null
  return rows.every((r) => r.album_id === first) ? first : null
}
