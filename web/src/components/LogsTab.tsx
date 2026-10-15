// 記録画面の「ログ」タブ（SPEC §12.4、D-106）。照合・吸い出しの実行ごとに 1 行。行を開くと本文を取る。
// 照合ログは album ごとに最新 1 回分しか残らないので、古い実行は本文を出さずにその旨を出す

import { Fragment, useEffect, useState } from 'react'
import { apiText } from '../api/client'
import { useLogs } from '../hooks/useLogs'
import { formatDateTime } from '../lib/history'
import { logAvailText, logErrorText, logFileName, logKindLabel, logSummary, type LogKind, type LogRun } from '../lib/records'
import { LogText } from './LogText'

const KIND_TABS: ReadonlyArray<readonly [LogKind, string]> = [
  ['all', 'すべて'],
  ['retro', '照合'],
  ['rip', '吸い出し'],
]

export function LogsTab({ albumId, onClearAlbum }: { albumId: number | null; onClearAlbum: () => void }) {
  const [kind, setKind] = useState<LogKind>('all')
  const { items, hasMore, loadMore, refresh, error } = useLogs(true, kind, albumId)
  // 利用者が行を開閉した結果。どの album 指定のもとでの操作かも持ち、album の指定が変わったら無効にする
  const [chosen, setChosen] = useState<{ albumId: number | null; key: string | null } | null>(null)
  // album を指定して来たときは、利用者が触るまで最新（先頭）を開く。閉じたら開き直さない
  const open =
    chosen != null && chosen.albumId === albumId ? chosen.key : albumId != null ? (items?.[0]?.key ?? null) : null
  const toggle = (key: string) => setChosen({ albumId, key: open === key ? null : key })
  return (
    <>
      <div className="table-toolbar">
        <div className="tabs" role="tablist">
          {KIND_TABS.map(([k, label]) => (
            <button
              key={k}
              type="button"
              role="tab"
              aria-selected={kind === k}
              className={kind === k ? 'active' : ''}
              onClick={() => setKind(k)}
            >
              {label}
            </button>
          ))}
        </div>
        {albumId != null && (
          <button type="button" className="ghost small" onClick={onClearAlbum}>
            album #{albumId} ×
          </button>
        )}
        <span className="spacer" />
        <button type="button" className="ghost" onClick={refresh}>
          更新
        </button>
      </div>
      {error != null && <p className="error">{error}</p>}
      {items == null ? (
        <p className="muted">読み込み中…</p>
      ) : items.length === 0 ? (
        <p className="muted">該当する記録は無い</p>
      ) : (
        <>
          <table className="history-table logs-table">
            <thead>
              <tr>
                <th>日時</th>
                <th>種別</th>
                <th>アルバム</th>
                <th>結果</th>
              </tr>
            </thead>
            <tbody>
              {items.map((r) => (
                <Fragment key={r.key}>
                  <tr className={open === r.key ? 'selected' : ''} onClick={() => toggle(r.key)}>
                    <td className="nowrap">{formatDateTime(r.verified_at)}</td>
                    <td className="nowrap">{logKindLabel(r.source)}</td>
                    <td>
                      {r.albumartist ?? ''} / {r.album ?? ''}
                    </td>
                    <td>{logSummary(r.discs)}</td>
                  </tr>
                  {open === r.key && (
                    <tr className="log-detail">
                      <td colSpan={4}>
                        <LogBody run={r} />
                      </td>
                    </tr>
                  )}
                </Fragment>
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

type Fetched = { url: string; text: string | null; error: string | null }

function LogBody({ run }: { run: LogRun }) {
  const discs = [...new Set(run.discs.map((d) => d.disc_no))]
  const [disc, setDisc] = useState<number | null>(discs[0] ?? null)
  const unavailable = logAvailText(run.log)
  const q = run.source === 'rip' && disc != null ? `?disc=${disc}` : ''
  const url = `/api/logs/${encodeURIComponent(run.key)}/text${q}`
  // 結果は取得した URL と一緒に持ち、今の URL の分だけを使う（disc 切り替えで前の本文を出さない）
  const [fetched, setFetched] = useState<Fetched | null>(null)
  useEffect(() => {
    if (unavailable != null) return
    let stale = false
    apiText(url)
      .then((t) => {
        if (!stale) setFetched({ url, text: t, error: null })
      })
      .catch((e: unknown) => {
        if (stale) return
        setFetched({ url, text: null, error: logErrorText(e) })
      })
    return () => {
      stale = true
    }
  }, [url, unavailable])
  if (unavailable != null) return <p className="muted small">{unavailable}</p>
  const cur = fetched != null && fetched.url === url ? fetched : null
  return (
    <>
      {run.source === 'rip' && discs.length > 1 && (
        <div className="tabs">
          {discs.map((d) => (
            <button key={String(d)} type="button" className={disc === d ? 'active' : ''} onClick={() => setDisc(d)}>
              disc {d ?? '?'}
            </button>
          ))}
        </div>
      )}
      {cur == null ? (
        <p className="muted small">読み込み中…</p>
      ) : cur.error != null ? (
        <p className="error small">{cur.error}</p>
      ) : (
        <LogText
          text={cur.text ?? ''}
          fileName={logFileName(run, disc, discs.length)}
        />
      )}
    </>
  )
}
