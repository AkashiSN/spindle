// YouTube 画面（SPEC §12.6、D-70 追記、P4-13）: URL を貼って ytdl ジョブに投入し、その行方（ジョブ →
// Inbox）を同じ画面で追う。購読の節は P4-16 で埋める

import type { JobsState } from '../hooks/useJobSummary'
import type { YoutubeState } from '../hooks/useYoutube'
import { formatDateTime } from '../lib/history'
import { canCancel, canRetry } from '../lib/jobs'
import { isPlaylistUrl, parseUrlLines, ytdlJobs, ytdlResultLabel } from '../lib/youtube'

export function YoutubeView({
  youtube,
  jobs,
  onOpenInbox,
}: {
  youtube: YoutubeState
  jobs: JobsState
  onOpenInbox: () => void
}) {
  const { urls, busy, notice, error } = youtube
  const list = jobs.items ? ytdlJobs(jobs.items) : null

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
        <button type="button" className="ghost" onClick={jobs.refresh}>
          更新
        </button>
      </div>
      {error != null && <p className="error">{error}</p>}

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
                  {j.state === 'done' && !isPlaylistUrl(j.subject) && (
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

      <h2>購読</h2>
      <p className="muted small">
        再生リストを登録しておくと、無いものだけを自動で取り込む（P4-16 で追加。いまは URL を貼る）
      </p>
    </section>
  )
}
