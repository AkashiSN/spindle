// 下部バー（SPEC §12.1）: 左が再生（P1-9）、右がジョブ要約「実行中 N · 反映待ち M · 失敗 K」

import type { JobSummary } from '../api/types'
import { formatCount } from '../lib/format'

export function BottomBar({
  summary,
  connected,
  onJobsClick,
}: {
  summary: JobSummary | null
  connected: boolean
  onJobsClick: () => void
}) {
  return (
    <footer className="bottom-bar">
      <div className="player muted small">▶ ‖ ──────── 再生は P1-9</div>
      <button type="button" className="ghost jobs-summary" onClick={onJobsClick} title="ジョブ画面へ">
        {summary ? (
          <>
            実行中 {formatCount(summary.running)} · 反映待ち {formatCount(summary.pending_ops)} · 失敗{' '}
            <span className={summary.failed > 0 ? 'error' : ''}>{formatCount(summary.failed)}</span>
            {summary.queued > 0 ? <span className="muted"> · 待ち {formatCount(summary.queued)}</span> : null}
          </>
        ) : (
          <span className="muted">ジョブ要約を取得中…</span>
        )}
        <span className={`dot ${connected ? 'on' : 'off'}`} title={connected ? 'SSE 接続中' : 'SSE 切断（再接続中）'} />
      </button>
    </footer>
  )
}
