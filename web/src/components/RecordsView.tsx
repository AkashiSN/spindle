// 記録画面（SPEC §12.4、D-106）: ☰ メニューの「記録」。編集履歴 / 退避ファイル / ログ をタブで切り替える。
// 他の画面からは App の openRecords でタブと対象（バッチ・album）を指定して開く

import type { HistoryState } from '../hooks/useHistory'
import { RECORDS_TABS, type RecordsTab } from '../lib/records'
import { ArchiveTab } from './ArchiveTab'
import { HistoryView } from './HistoryView'

export function RecordsView({
  tab,
  onTab,
  history,
  historyFocus,
  onOpenBatch,
}: {
  tab: RecordsTab
  onTab: (t: RecordsTab) => void
  history: HistoryState
  historyFocus: number | null
  onOpenBatch: (batchId: number) => void
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
      {tab === 'history' ? (
        <HistoryView history={history} focusId={historyFocus} />
      ) : tab === 'archive' ? (
        <ArchiveTab onOpenBatch={onOpenBatch} />
      ) : (
        <p className="muted">ログ</p>
      )}
    </section>
  )
}
