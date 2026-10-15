// 記録画面の「退避ファイル」タブ（SPEC §12.4、D-58、D-106）。既定は保持中。復元は対応バッチを
// 編集履歴から巻き戻す

import { useState } from 'react'
import { useArchive } from '../hooks/useArchive'
import { formatCount } from '../lib/format'
import { formatDateTime } from '../lib/history'
import { archiveEmptyText, archiveFilterTabs, type ArchiveFilter } from '../lib/records'
import { archiveReasonLabel, archiveStateLabel } from '../lib/settings'

export function ArchiveTab({ onOpenBatch }: { onOpenBatch: (batchId: number) => void }) {
  const [filter, setFilter] = useState<ArchiveFilter>('held')
  const { items, counts, hasMore, loadMore, refresh, error } = useArchive(true, filter)
  return (
    <>
      <div className="table-toolbar">
        <div className="tabs" role="tablist">
          {counts &&
            archiveFilterTabs(counts).map((t) => (
              <button
                key={t.key}
                type="button"
                role="tab"
                aria-selected={filter === t.key}
                className={filter === t.key ? 'active' : ''}
                onClick={() => setFilter(t.key)}
              >
                {t.label}
                <span className="count">{formatCount(t.count)}</span>
              </button>
            ))}
        </div>
        <span className="spacer" />
        <button type="button" className="ghost" onClick={refresh}>
          更新
        </button>
      </div>
      <p className="muted small">正規化で置き換えた元ファイルを Archive/ に退避した記録。物理削除は GC が保持期間の後に行う</p>
      {error != null && <p className="error">{error}</p>}
      {items == null ? (
        <p className="muted">読み込み中…</p>
      ) : items.length === 0 ? (
        <p className="muted">{counts ? archiveEmptyText(filter, counts) : ''}</p>
      ) : (
        <>
          <table className="history-table archive-table">
            <thead>
              <tr>
                <th className="num">#</th>
                <th>元のパス</th>
                <th>退避先（Archive/）</th>
                <th>理由</th>
                <th>退避</th>
                <th>GC 期限</th>
                <th>状態</th>
                <th>復元</th>
              </tr>
            </thead>
            <tbody>
              {items.map((a) => (
                <tr key={a.id} className={`state-${a.state}`}>
                  <td className="num">{a.id}</td>
                  <td className="path">{a.source_rel_path}</td>
                  <td className="path">{a.rel_path}</td>
                  <td className="nowrap">{archiveReasonLabel(a.reason)}</td>
                  <td className="nowrap">{formatDateTime(a.archived_at)}</td>
                  <td className="nowrap">{formatDateTime(a.eligible_after)}</td>
                  <td className="nowrap">
                    {archiveStateLabel(a.state)}
                    {a.state_at != null && <span className="muted small"> {formatDateTime(a.state_at)}</span>}
                  </td>
                  <td className="nowrap">
                    {a.state === 'held' && a.batch_id != null ? (
                      <button type="button" className="link small" onClick={() => onOpenBatch(a.batch_id!)}>
                        バッチ #{a.batch_id} を巻き戻す
                      </button>
                    ) : (
                      ''
                    )}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
          {hasMore && (
            <button type="button" className="ghost load-more" onClick={loadMore}>
              さらに読み込む
            </button>
          )}
        </>
      )}
    </>
  )
}
