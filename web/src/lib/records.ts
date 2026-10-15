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
