// ジョブ画面（SPEC §12.5）: 種別ごとの並列度と待ち行列、一覧（進捗 / 失敗の last_error / 再試行 / 取り消し）。
// 編集バッチ由来のジョブは edit_batch_id で履歴画面へ。更新は SSE job / batch（hooks/useJobSummary）、
// リロードしても DB の値で復元

import { useState } from 'react'
import type { Job } from '../api/types'
import type { JobsState } from '../hooks/useJobSummary'
import { formatCount } from '../lib/format'
import { formatDateTime } from '../lib/history'
import {
  canCancel,
  canRetry,
  cpuBudgetLabel,
  filterJobs,
  jobProgress,
  jobTypeLabel,
  STATE_LABEL,
  summarizeByType,
  tabCounts,
  type StateFilter,
} from '../lib/jobs'

/** `GET /api/jobs` の一覧の上限（src/jobs/queue.rs の LIST_LIMIT）。これに達していれば切れている */
const LIST_LIMIT = 1000

export function JobsView({ jobs, onOpenBatch }: { jobs: JobsState; onOpenBatch: (batchId: number) => void }) {
  const [state, setState] = useState<StateFilter>('active')
  const [type, setType] = useState<string | null>(null)
  const { items, summary, concurrency, byType: counts, cpuBudget, error, notice } = jobs

  const byType = summarizeByType(counts, concurrency)
  const shown = items ? filterJobs(items, state, type) : []
  const tabs = summary ? tabCounts(summary) : null
  // 一覧はサーバの上限で切れる（実行中 → 待ち → 終端の順なので、切れるのは待ちの末尾と古い終端）
  const truncated = items != null && items.length >= LIST_LIMIT

  return (
    <section className="jobs">
      <div className="table-toolbar">
        <h1>ジョブ</h1>
        {summary && (
          <span className="small muted">
            実行中 {formatCount(summary.running)} · 待ち {formatCount(summary.queued)} · 完了 {formatCount(summary.done)} ·
            失敗 {formatCount(summary.failed)} · 反映待ち op {formatCount(summary.pending_ops)}
          </span>
        )}
        <span className="spacer" />
        {notice != null && <span className="small">{notice}</span>}
        <button type="button" className="ghost" onClick={jobs.refresh}>
          更新
        </button>
      </div>
      {error != null && <p className="error">{error}</p>}

      <table className="jobs-types">
        <thead>
          <tr>
            <th>種別</th>
            <th className="num">並列度</th>
            <th className="num">待ち</th>
            <th className="num">実行中</th>
            <th className="num">完了</th>
            <th className="num">失敗</th>
          </tr>
        </thead>
        <tbody>
          {byType.map((t) => (
            <tr
              key={t.type}
              className={type === t.type ? 'selected' : ''}
              onClick={() => setType(type === t.type ? null : t.type)}
              title="クリックで一覧をこの種別に絞る"
            >
              <td>
                {jobTypeLabel(t.type)} <span className="muted small">{t.type}</span>
              </td>
              <td className="num">{t.concurrency ?? '–'}</td>
              <td className="num">{t.queued ? formatCount(t.queued) : ''}</td>
              <td className="num">{t.running ? formatCount(t.running) : ''}</td>
              <td className="num">{t.done ? formatCount(t.done) : ''}</td>
              <td className={`num${t.failed > 0 ? ' failed' : ''}`}>{t.failed ? formatCount(t.failed) : ''}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {cpuBudgetLabel(byType, cpuBudget) != null && (
        <p className="muted small jobs-cpu-budget">{cpuBudgetLabel(byType, cpuBudget)}</p>
      )}

      <div className="table-toolbar">
        <div className="tabs" role="tablist">
          {(
            [
              ['active', '実行中・待ち'],
              ['done', '完了'],
              ['failed', '失敗・取り消し'],
              ['all', 'すべて'],
            ] as const
          ).map(([k, label]) => (
            <button
              key={k}
              type="button"
              role="tab"
              aria-selected={state === k}
              className={state === k ? 'active' : ''}
              onClick={() => setState(k)}
            >
              {label}
              {tabs && <span className="count">{formatCount(tabs[k])}</span>}
            </button>
          ))}
        </div>
        {type != null && (
          <button type="button" className="ghost small" onClick={() => setType(null)}>
            種別: {jobTypeLabel(type)} ×
          </button>
        )}
        <span className="spacer" />
        <span className="muted small">
          {items ? `${formatCount(shown.length)} 件${truncated ? `（表示は最新 ${formatCount(LIST_LIMIT)} 件まで）` : ''}` : ''}
        </span>
      </div>
      {items == null ? (
        <p className="muted">読み込み中…</p>
      ) : shown.length === 0 ? (
        <p className="muted">該当するジョブはありません</p>
      ) : (
        <table className="history-table jobs-table">
          <thead>
            <tr>
              <th className="num">#</th>
              <th>種別</th>
              <th>状態</th>
              <th className="num">進捗</th>
              <th>作成</th>
              <th>開始</th>
              <th>終了</th>
              <th className="num">試行</th>
              <th>エラー</th>
              <th></th>
            </tr>
          </thead>
          <tbody>
            {shown.map((j) => (
              <JobRow key={j.id} j={j} jobs={jobs} onOpenBatch={onOpenBatch} />
            ))}
          </tbody>
        </table>
      )}
    </section>
  )
}

function JobRow({ j, jobs, onOpenBatch }: { j: Job; jobs: JobsState; onOpenBatch: (batchId: number) => void }) {
  // 再試行のバックオフ待ち（run_after）。過ぎていればワーカーがすぐ拾う
  const runAfter = j.state === 'queued' && j.run_after != null
  return (
    <tr className={`state-${j.state}`}>
      <td className="num">#{j.id}</td>
      <td>
        {jobTypeLabel(j.type)}
        {j.edit_batch_id != null && (
          <>
            {' '}
            <button
              type="button"
              className="link small"
              title="編集履歴で開く"
              onClick={() => onOpenBatch(j.edit_batch_id!)}
            >
              バッチ #{j.edit_batch_id}
            </button>
          </>
        )}
      </td>
      <td className="nowrap">
        {STATE_LABEL[j.state]}
        {runAfter && <span className="muted small"> （{formatDateTime(j.run_after!)} から）</span>}
      </td>
      <td className="num">{jobProgress(j)}</td>
      <td className="nowrap">{formatDateTime(j.created_at)}</td>
      <td className="nowrap">{j.started_at != null ? formatDateTime(j.started_at) : ''}</td>
      <td className="nowrap">{j.finished_at != null ? formatDateTime(j.finished_at) : ''}</td>
      <td className="num">
        {j.attempts} / {j.max_attempts}
      </td>
      <td className="error-cell" title={j.last_error ?? undefined}>
        {j.last_error}
      </td>
      <td className="nowrap">
        {canCancel(j) && (
          <button type="button" className="ghost" onClick={() => void jobs.cancel(j.id)}>
            取り消し
          </button>
        )}
        {canRetry(j) && (
          <button type="button" className="ghost" onClick={() => void jobs.retry(j.id)}>
            再試行
          </button>
        )}
      </td>
    </tr>
  )
}
