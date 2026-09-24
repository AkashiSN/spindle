// YouTube 画面（SPEC §12.6、D-70 追記、P4-13、D-87）。上部の切り替えで「ダウンロード」と「購読」。
// ダウンロードは番号付きの段: ① URL を貼る（種類とライブラリ / Inbox の所在を照合）→ ② 取り込み方 →
// ③ ダウンロード（今回の投入のジョブと、再生リストの展開で増えた子）→ ④ Inbox で承認（置いた件を開く）。これまでのジョブは折りたたみ。
// 購読の節（P4-16）は SubscriptionsSection

import { useMemo, useState } from 'react'
import type { JobsState } from '../hooks/useJobSummary'
import type { SubscriptionsState } from '../hooks/useSubscriptions'
import { useUrlLookup } from '../hooks/useUrlLookup'
import type { YoutubeState } from '../hooks/useYoutube'
import { formatDateTime } from '../lib/history'
import { canCancel, canRetry } from '../lib/jobs'
import {
  isFinished,
  parseUrlLines,
  sessionJobs,
  stagedDir,
  stagedDirs,
  urlRows,
  urlRowsSummary,
  ytdlResultLabel,
} from '../lib/youtube'
import type { Job } from '../api/types'
import { Step } from './Step'
import { SubscriptionsSection } from './SubscriptionsSection'

type Mode = 'download' | 'subscriptions'

export function YoutubeView({
  youtube,
  subs,
  jobs,
  onOpenInbox,
}: {
  youtube: YoutubeState
  subs: SubscriptionsState
  jobs: JobsState
  /** Inbox を開く。`dir` があればその件を選ぶ */
  onOpenInbox: (dir?: string) => void
}) {
  const [mode, setMode] = useState<Mode>('download')
  // 「購読にする →」で購読の ① に渡す URL（渡すたびにフォームを作り直す）
  const [prefill, setPrefill] = useState<{ url: string; n: number } | null>(null)
  const { notice, error } = youtube

  return (
    <section className="youtube">
      <div className="table-toolbar">
        <h1>YouTube</h1>
        <div className="seg" role="tablist" aria-label="YouTube の作業">
          <button
            type="button"
            role="tab"
            aria-selected={mode === 'download'}
            className={mode === 'download' ? 'on' : ''}
            onClick={() => setMode('download')}
          >
            ダウンロード
          </button>
          <button
            type="button"
            role="tab"
            aria-selected={mode === 'subscriptions'}
            className={mode === 'subscriptions' ? 'on' : ''}
            onClick={() => setMode('subscriptions')}
          >
            購読{subs.items != null && subs.items.length > 0 && <span className="badge">{subs.items.length}</span>}
          </button>
        </div>
        <span className="spacer" />
        {notice != null && (
          <span className="small">
            {notice}{' '}
            <button type="button" className="ghost" onClick={youtube.clearNotice} title="閉じる">
              ×
            </button>
          </span>
        )}
        <button
          type="button"
          className="ghost"
          onClick={() => {
            youtube.refresh()
            subs.refresh()
          }}
        >
          更新
        </button>
      </div>
      {error != null && <p className="error">{error}</p>}
      {subs.error != null && <p className="error">{subs.error}</p>}

      {mode === 'download' ? (
        <DownloadSteps
          youtube={youtube}
          jobs={jobs}
          onOpenInbox={onOpenInbox}
          onSubscribe={(url) => {
            setPrefill((p) => ({ url, n: (p?.n ?? 0) + 1 }))
            setMode('subscriptions')
          }}
        />
      ) : (
        <SubscriptionsSection key={prefill?.n ?? 0} subs={subs} initialUrl={prefill?.url ?? ''} />
      )}
    </section>
  )
}

function DownloadSteps({
  youtube,
  jobs,
  onOpenInbox,
  onSubscribe,
}: {
  youtube: YoutubeState
  jobs: JobsState
  onOpenInbox: (dir?: string) => void
  onSubscribe: (url: string) => void
}) {
  const { urls, busy } = youtube
  const list = useMemo(() => parseUrlLines(urls), [urls])
  const { lookup, probes, error: lookupError } = useUrlLookup(list)
  const rows = useMemo(() => urlRows(list, lookup, probes), [list, lookup, probes])
  const targets = rows.filter((r) => !r.skip).map((r) => r.url)
  const skipped = rows.length - targets.length
  const unsubscribed = list.find((u) => {
    const it = lookup.get(u)
    return it?.kind === 'playlist' && it.subscription == null
  })

  const session = useMemo(() => sessionJobs(youtube.jobs, youtube.sessionIds), [youtube.jobs, youtube.sessionIds])
  const finished = session.filter(isFinished).length
  const running = session.length > 0 && finished < session.length
  const staged = useMemo(() => stagedDirs(session), [session])
  const history = youtube.jobs ?? []
  const hasInput = list.length > 0

  return (
    <>
      <Step
        no={1}
        title="URL を貼る"
        aside={urlRowsSummary(list, lookup)}
        done={targets.length > 0}
        hint="動画・再生リストの URL を 1 行に 1 つ。music.youtube.com と youtu.be も可。貼ると種類を判定し、ライブラリと Inbox にもう有るか（SOURCE_URL の一致）を示す。再生リストは中身を yt-dlp で調べるので数秒かかる"
      >
        <textarea
          className="youtube-urls"
          aria-label="YouTube の URL（1 行 1 つ）"
          placeholder={'動画か再生リストの URL を 1 行に 1 つ'}
          rows={4}
          value={urls}
          disabled={busy}
          onChange={(e) => youtube.setUrls(e.target.value)}
        />
        {lookupError != null && <p className="error small">照合できない: {lookupError}</p>}
        {rows.length > 0 && (
          <table className="history-table youtube-urls-table">
            <thead>
              <tr>
                <th>種類</th>
                <th>URL</th>
                <th>いまの状態</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.url} className={`tone-${r.tone}`}>
                  <td className="nowrap">{r.kindLabel}</td>
                  <td className="path">{r.url}</td>
                  <td>
                    <span className={`badge yt-${r.tone}`}>{r.badge}</span> {r.detail}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </Step>

      <Step
        no={2}
        title="取り込み方"
        aside="確かめるだけ。変えるものは無い"
        wait={!hasInput}
        done={targets.length > 0}
        hint="行き先は常に Inbox。承認するまでライブラリには入らない。アルバムの判定はメタデータプラグイン"
      >
        <table className="kv">
          <tbody>
            <tr>
              <th>行き先</th>
              <td>Inbox（承認してからライブラリへ配置）</td>
            </tr>
            <tr>
              <th>アルバムの判定</th>
              <td>
                メタデータプラグインで判定。判定できない動画は <code>youtube/_unmatched</code> に置く
              </td>
            </tr>
            <tr>
              <th>再生リスト</th>
              <td>動画ごとに展開し、取り込み済み（SOURCE_URL が一致）の動画は飛ばす</td>
            </tr>
            <tr>
              <th>カバー画像</th>
              <td>動画ごとのサムネイルを曲ごとに埋め込む（Inbox で差し替え・そろえられる）</td>
            </tr>
          </tbody>
        </table>
        {unsubscribed != null && (
          <p className="yt-banner small">
            再生リストが含まれる。今後の追加も取りたいなら{' '}
            <button type="button" className="ghost" onClick={() => onSubscribe(unsubscribed)}>
              購読にする →
            </button>
          </p>
        )}
      </Step>

      <Step
        no={3}
        title="ダウンロード"
        aside={session.length > 0 ? (running ? `${finished} / ${session.length} 件済み` : `${session.length} 件済み`) : undefined}
        wait={targets.length === 0 && session.length === 0}
        done={session.length > 0 && !running}
      >
        <div className="op-row">
          <button
            type="button"
            className="primary"
            disabled={busy || targets.length === 0}
            onClick={() => void youtube.start(targets)}
          >
            {busy ? '投入中…' : targets.length > 0 ? `${targets.length} 件をダウンロード` : 'ダウンロード'}
          </button>
          <span className="muted small">
            {hasInput && targets.length === 0
              ? '新しく取るものが無い（全部飛ばす）'
              : skipped > 0
                ? `飛ばす ${skipped} 行は投入しない`
                : ''}
          </span>
        </div>
        {session.length > 0 && <JobTable list={session} jobs={jobs} onOpenInbox={onOpenInbox} />}
      </Step>

      <Step
        no={4}
        title="Inbox で承認"
        aside={staged.length > 0 ? `Inbox に ${staged.length} 件。開いて ①〜④ で承認する` : 'ダウンロードが済むとここに並ぶ'}
        wait={staged.length === 0}
      >
        {staged.length === 0 ? (
          <p className="muted small">まだ無い</p>
        ) : (
          <ul className="yt-staged">
            {staged.map((s) => (
              <li key={s.dir}>
                <span className="path">{s.dir === '' ? '(Inbox 直下)' : s.dir}</span>
                <span className="muted small">{s.count} 曲</span>
                <button type="button" onClick={() => onOpenInbox(s.dir)}>
                  Inbox で開く
                </button>
              </li>
            ))}
          </ul>
        )}
      </Step>

      <details className="yt-history">
        <summary>これまでのダウンロード（ジョブ {history.length} 件）</summary>
        {youtube.jobs == null ? (
          <p className="muted">読み込み中…</p>
        ) : history.length === 0 ? (
          <p className="muted">ダウンロードのジョブはまだ無い</p>
        ) : (
          <JobTable list={history} jobs={jobs} onOpenInbox={onOpenInbox} withTime />
        )}
      </details>
    </>
  )
}

function JobTable({
  list,
  jobs,
  onOpenInbox,
  withTime = false,
}: {
  list: Job[]
  jobs: JobsState
  onOpenInbox: (dir?: string) => void
  withTime?: boolean
}) {
  return (
    <table className="history-table youtube-jobs">
      <thead>
        <tr>
          <th>#</th>
          <th>URL</th>
          <th>結果</th>
          {withTime && <th>時刻</th>}
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
            {withTime && <td className="nowrap muted">{formatDateTime(j.finished_at ?? j.started_at ?? j.created_at)}</td>}
            <td className="nowrap">
              {j.state === 'done' && j.note != null && j.note.startsWith('Inbox に置いた') && (
                <button type="button" className="ghost" onClick={() => onOpenInbox(stagedDir(j) ?? undefined)}>
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
  )
}
