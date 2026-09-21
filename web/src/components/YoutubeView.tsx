// YouTube 画面（SPEC §12.6、D-70 追記、P4-13）: URL を貼って ytdl ジョブに投入し、その行方（ジョブ →
// Inbox）を同じ画面で追う。購読の節（P4-16）は SubscriptionsSection

import type { JobsState } from '../hooks/useJobSummary'
import type { SubscriptionsState } from '../hooks/useSubscriptions'
import type { YoutubeState } from '../hooks/useYoutube'
import { formatDateTime } from '../lib/history'
import { canCancel, canRetry } from '../lib/jobs'
import { parseUrlLines, ytdlResultLabel } from '../lib/youtube'
import { SubscriptionsSection } from './SubscriptionsSection'

export function YoutubeView({
  youtube,
  subs,
  jobs,
  onOpenInbox,
}: {
  youtube: YoutubeState
  subs: SubscriptionsState
  jobs: JobsState
  onOpenInbox: () => void
}) {
  const { urls, busy, notice, error } = youtube
  const list = youtube.jobs

  return (
    <section className="youtube">
      <div className="table-toolbar">
        <h1>YouTube</h1>
        <span className="spacer" />
        {notice != null && (
          <span className="small">
            {notice}{' '}
            <button type="button" className="ghost" onClick={youtube.clearNotice} title="閉じる">
              ×
            </button>
          </span>
        )}
        <button type="button" className="ghost" onClick={youtube.refresh}>
          更新
        </button>
      </div>
      {error != null && <p className="error">{error}</p>}
      {subs.error != null && <p className="error">{subs.error}</p>}

      <h2>ダウンロード</h2>
      <textarea
        className="youtube-urls"
        aria-label="YouTube の URL（1 行 1 つ）"
        placeholder={'動画か再生リストの URL を 1 行に 1 つ'}
        rows={4}
        value={urls}
        disabled={busy}
        onChange={(e) => youtube.setUrls(e.target.value)}
      />
      <div className="op-row">
        <button
          type="button"
          className="primary"
          disabled={busy || parseUrlLines(urls).length === 0}
          onClick={() => void youtube.start(parseUrlLines(urls))}
        >
          {busy ? '投入中…' : 'ダウンロード'}
        </button>
        <span className="muted small">
          音声を取って Inbox に置く（メタデータプラグインの判定付き。判定できないものは youtube/_unmatched）。再生リストは
          動画ごとに展開し、取り込み済み（SOURCE_URL が一致）の動画は飛ばす。承認は Inbox
        </span>
      </div>

      <SubscriptionsSection subs={subs} />

      <h2>ジョブ</h2>
      {list == null ? (
        <p className="muted">読み込み中…</p>
      ) : list.length === 0 ? (
        <p className="muted">ダウンロードのジョブはまだ無い</p>
      ) : (
        <table className="history-table youtube-jobs">
          <thead>
            <tr>
              <th>#</th>
              <th>URL</th>
              <th>結果</th>
              <th>時刻</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {list.map((j) => (
              <tr key={j.id} className={j.state === 'failed' ? 'failed' : undefined}>
                <td className="nowrap">#{j.id}</td>
                <td className="path">
                  {j.subject != null ? (
                    <a href={j.subject} target="_blank" rel="noreferrer noopener">
                      {j.subject}
                    </a>
                  ) : (
                    '—'
                  )}
                </td>
                <td>{ytdlResultLabel(j)}</td>
                <td className="nowrap muted">{formatDateTime(j.finished_at ?? j.started_at ?? j.created_at)}</td>
                <td className="nowrap">
                  {j.state === 'done' && j.note != null && j.note.startsWith('Inbox に置いた') && (
                    <button type="button" className="ghost" onClick={onOpenInbox}>
                      Inbox で確認
                    </button>
                  )}
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
            ))}
          </tbody>
        </table>
      )}

    </section>
  )
}
