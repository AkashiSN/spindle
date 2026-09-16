// 編集履歴画面（SPEC §12.4）: バッチ一覧、行を開いて op と conflict の現在値、[巻き戻す] /
// [キャンセル]、戻し済みの注記、↩ で逆バッチの関係

import { Fragment, useState } from 'react'
import type { HistoryDetail, HistoryItem, OpView } from '../api/types'
import type { HistoryState } from '../hooks/useHistory'
import { formatCount } from '../lib/format'
import {
  batchLabel,
  canCancel,
  canRevert,
  formatDateTime,
  formatValue,
  revertedNote,
  stateLabel,
} from '../lib/history'

export function HistoryView({ history }: { history: HistoryState }) {
  const [openId, setOpenId] = useState<number | null>(null)
  const { items, error, details, notice } = history

  const toggle = (id: number) => {
    if (openId === id) {
      history.close(id)
      setOpenId(null)
    } else {
      if (openId != null) history.close(openId)
      history.open(id)
      setOpenId(id)
    }
  }

  return (
    <section className="history">
      <div className="table-toolbar">
        <h1>編集履歴</h1>
        <span className="spacer" />
        {notice != null && <span className="small">{notice}</span>}
        <button type="button" className="ghost" onClick={history.refresh}>
          更新
        </button>
      </div>
      {error != null && <p className="error">{error}</p>}
      {items == null ? (
        <p className="muted">読み込み中…</p>
      ) : items.length === 0 ? (
        <p className="muted">まだ編集はありません</p>
      ) : (
        <table className="history-table">
          <thead>
            <tr>
              <th>#</th>
              <th>日時</th>
              <th>説明</th>
              <th>種別</th>
              <th className="num">件数</th>
              <th>状態</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {items.map((b) => (
              <Fragment key={b.id}>
                <BatchRow
                  b={b}
                  open={openId === b.id}
                  onToggle={() => toggle(b.id)}
                  onRevert={() => history.revert(b.id)}
                  onCancel={() => history.cancel(b.id)}
                />
                {openId === b.id && (
                  <tr className="detail">
                    <td colSpan={7}>
                      <BatchDetail detail={details[b.id]} />
                    </td>
                  </tr>
                )}
              </Fragment>
            ))}
          </tbody>
        </table>
      )}
    </section>
  )
}

function BatchRow({
  b,
  open,
  onToggle,
  onRevert,
  onCancel,
}: {
  b: HistoryItem
  open: boolean
  onToggle: () => void
  onRevert: () => void
  onCancel: () => void
}) {
  const note = revertedNote(b)
  const isRedo = b.reverts_batch_id != null
  return (
    <tr className={open ? 'open' : undefined} onClick={onToggle}>
      <td className="num">#{b.id}</td>
      <td className="nowrap">{formatDateTime(b.created_at)}</td>
      <td>
        {batchLabel(b)}
        {isRedo && (
          <span className="muted small" title={`#${b.reverts_batch_id} の巻き戻し`}>
            {' '}
            ↩ #{b.reverts_batch_id}
          </span>
        )}
      </td>
      <td className="nowrap">{b.kind ?? ''}</td>
      <td className="num">{formatCount(b.affected)} 件</td>
      <td className="nowrap">
        {stateLabel(b)}
        {note != null && <span className="muted small"> {note}</span>}
      </td>
      <td className="actions" onClick={(e) => e.stopPropagation()}>
        {canCancel(b) && (
          <button type="button" onClick={onCancel}>
            キャンセル
          </button>
        )}
        {canRevert(b) && (
          <button type="button" onClick={onRevert}>
            {isRedo ? '巻き戻す (=やり直し)' : '巻き戻す'}
          </button>
        )}
        {note != null && <span className="muted small">—</span>}
      </td>
    </tr>
  )
}

function BatchDetail({ detail }: { detail: HistoryDetail | undefined }) {
  if (detail == null) return <p className="muted small">読み込み中…</p>
  return (
    <table className="ops-table">
      <thead>
        <tr>
          <th>track</th>
          <th>パス</th>
          <th>result</th>
          <th>変更</th>
          <th>error / 現在値</th>
        </tr>
      </thead>
      <tbody>
        {detail.ops.map((op) => (
          <OpRow key={op.id} op={op} />
        ))}
      </tbody>
    </table>
  )
}

function OpRow({ op }: { op: OpView }) {
  const keys = Object.keys(op.edits)
  return (
    <tr className={op.result === 'skipped_conflict' ? 'conflict' : undefined}>
      <td className="num">{op.track_id}</td>
      <td className="path">{op.rel_path ?? '(削除済み)'}</td>
      <td className="nowrap">{op.result}</td>
      <td>
        {keys.map((k) => (
          <div key={k}>
            <span className="muted">{k}:</span> {formatValue(op.edits[k].old)} → {formatValue(op.edits[k].new)}
          </div>
        ))}
      </td>
      <td>
        {op.error != null && <div className="error small">{op.error}</div>}
        {op.current != null && (
          <div className="small">
            <span className="muted">ファイルを再読込した現在値:</span>
            {Object.keys(op.current).map((k) => (
              <div key={k}>
                <span className="muted">{k}:</span> {formatValue(op.current?.[k])}
              </div>
            ))}
          </div>
        )}
      </td>
    </tr>
  )
}
