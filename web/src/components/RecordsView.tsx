// 記録画面（SPEC §12.4、D-106）: ☰ メニューの「記録」。編集履歴 / 退避ファイル / ログ をタブで切り替える。
// 他の画面からは App の openRecords でタブと対象（バッチ・ログの album）を指定して開く

import type { HistoryState } from '../hooks/useHistory'
import { RECORDS_TABS, type RecordsTab } from '../lib/records'
import { ArchiveTab } from './ArchiveTab'
import { HistoryView } from './HistoryView'
import { LogsTab } from './LogsTab'

export function RecordsView({
  tab,
  onTab,
  history,
  historyFocus,
  onOpenBatch,
  logsAlbum,
  onClearLogsAlbum,
}: {
  tab: RecordsTab
  onTab: (t: RecordsTab) => void
  history: HistoryState
  historyFocus: number | null
  onOpenBatch: (batchId: number) => void
  /** ログタブを絞る album（ジョブ・照合結果・プロパティから開いたとき） */
  logsAlbum: number | null
  onClearLogsAlbum: () => void
}) {
  return (
    <section className="records">
      <div className="tabs records-tabs" role="tablist">
        {RECORDS_TABS.map(([k, label]) => (
          <button
            key={k}
            type="button"
            role="tab"
            aria-selected={tab === k}
            className={tab === k ? 'active' : ''}
            onClick={() => onTab(k)}
          >
            {label}
          </button>
        ))}
      </div>
      {/* タブの行は固定し、中身だけをスクロールする（ほかの画面の section と同じく main の高さに収める） */}
      <div className="records-body">
        {tab === 'history' ? (
          <HistoryView history={history} focusId={historyFocus} />
        ) : tab === 'archive' ? (
          <ArchiveTab onOpenBatch={onOpenBatch} />
        ) : (
          <LogsTab albumId={logsAlbum} onClearAlbum={onClearLogsAlbum} />
        )}
      </div>
    </section>
  )
}
