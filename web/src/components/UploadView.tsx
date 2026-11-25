// アップロード画面（SPEC §12.6「アップロード」）。番号付きの段:
// ① ファイルを選ぶ（選んだ時点で送る）→ ② 送信（ファイルごとの進捗・zip の展開）→ ③ Inbox での分け方
// （件の表とチェック・まとめる）→ ④ Inbox へ置く → ⑤ Inbox で承認（置いた件を開く）。上に前回の続きの帯

import { Fragment, useEffect, useRef, useState, type ReactNode } from 'react'
import type { UploadGroupView, UploadMemberView, UploadOriginView, UploadView as View } from '../api/types'
import type { SendItem, UploadState } from '../hooks/useUpload'
import { formatBytes } from '../lib/settings'
import {
  ACCEPT_EXTS,
  canDiscard,
  canPublish,
  groupLabel,
  originState,
  publishedDirs,
  publishing,
  publishLabel,
  resumeBanner,
  resumeCandidates,
  statusLabel,
  statusTone,
  toggleFileIn,
  toggleGroupIn,
  unpackPending,
  type Tone,
} from '../lib/upload'
import { Step } from './Step'

const ACCEPT = ACCEPT_EXTS.map((e) => `.${e}`).join(',')

export function UploadView({
  upload,
  onOpenInbox,
}: {
  upload: UploadState
  /** Inbox を開いてその件を選ぶ */
  onOpenInbox: (dir?: string) => void
}) {
  const { view, error } = upload
  const frozen = view != null && (view.publish_requested_at != null || view.publish != null)
  const placed = view != null ? publishedDirs(view) : []
  const allPlaced = view?.publish != null && placed.length === view.publish.groups.length

  return (
    <section className="youtube upload">
      <div className="table-toolbar">
        <h1>アップロード</h1>
        <span className="spacer" />
        {view != null && !upload.sending && (
          <button
            type="button"
            className="ghost"
            disabled={upload.mutating}
            onClick={upload.startNew}
            title="開いているものは前回の続きに残る"
          >
            別に送る
          </button>
        )}
        <button type="button" className="ghost" onClick={upload.refresh}>
          更新
        </button>
      </div>
      {error != null && (
        <p className="error">
          {error}{' '}
          <button type="button" className="ghost" onClick={upload.clearError} title="閉じる">
            ×
          </button>
        </p>
      )}

      <ResumeBanners upload={upload} />

      <Step
        no={1}
        title="ファイルを選ぶ"
        done={view != null && view.origins.length > 0}
        hint="選んだ（ドロップした）時点で送り始める。送信中や ③ を見ている間も足せる（同じアップロードに足して ③ を取り直す）"
      >
        <DropZone
          upload={upload}
          disabled={frozen && !allPlaced}
          why={view?.publish_error != null ? '置き直すか捨ててから送る' : 'Inbox へ置いている間は足せない'}
        />
      </Step>

      <Step
        no={2}
        title="送信"
        wait={view == null || view.origins.length === 0}
        done={view != null && view.origins.length > 0 && !upload.sending && !unpackPending(view)}
        aside={upload.sending ? '送信中' : undefined}
      >
        <SendTable upload={upload} frozen={frozen} />
        <p className="muted small">送信中に画面を離れても、送り終えた分は残る（途中のファイルは送り直し）</p>
      </Step>

      <Step no={3} title="Inbox での分け方" wait={view == null || (view.groups.length === 0 && !frozen)} done={frozen}>
        {view == null ? (
          <p className="muted small">送り終えると、Inbox にどう置くかがここに並ぶ</p>
        ) : frozen ? (
          <FrozenPlan v={view} />
        ) : (
          <GroupTable v={view} upload={upload} />
        )}
      </Step>

      <Step no={4} title="Inbox へ置く" wait={view == null || view.groups.length === 0 && !frozen} done={allPlaced}>
        {view == null ? <p className="muted small">まだ置くものが無い</p> : <PublishStep v={view} upload={upload} />}
      </Step>

      <Step
        no={5}
        title="Inbox で承認"
        wait={placed.length === 0}
        aside={placed.length > 0 ? `Inbox に ${placed.length} 件。開いて ①〜④ で承認する` : '置き終えるとここに並ぶ'}
      >
        {placed.length === 0 ? (
          <p className="muted small">まだ無い</p>
        ) : (
          <ul className="yt-staged">
            {placed.map((p) => (
              <li key={p.key}>
                <span>{p.label}</span>
                <span className="path muted">{p.dir}</span>
                <span className="muted small">{p.tracks} 曲</span>
                <button type="button" onClick={() => onOpenInbox(p.dir)}>
                  Inbox で開く
                </button>
              </li>
            ))}
          </ul>
        )}
      </Step>
    </section>
  )
}

function Pill({ tone, children }: { tone: Tone; children: ReactNode }) {
  return <span className={`badge up-${tone}`}>{children}</span>
}

/** 前回の続き（Inbox へまだ置いていないもの。新しい順）と、壊れたアップロード（捨てるだけ） */
function ResumeBanners({ upload }: { upload: UploadState }) {
  const list = upload.list
  if (list == null) return null
  const items = resumeCandidates(list.items)
  if (items.length === 0 && list.broken.length === 0) return null
  return (
    <div className="upload-resume">
      {items.map((v) => (
        <p key={v.id} className="yt-banner small">
          {resumeBanner([v])}{' '}
          <button type="button" className="ghost" disabled={upload.sending || upload.mutating} onClick={() => upload.open(v.id)}>
            続きを開く
          </button>{' '}
          <button
            type="button"
            className="ghost"
            disabled={!canDiscard(v)}
            title={canDiscard(v) ? undefined : '展開か公開が動いている間は捨てられない'}
            onClick={() => {
              if (window.confirm('送ったものを捨てますか？（Inbox には置かない）')) void upload.discard(v.id)
            }}
          >
            捨てる
          </button>
        </p>
      ))}
      {list.broken.map((b) => (
        <p key={b.dir} className="yt-banner warn small">
          壊れたアップロード <span className="path">{b.dir}</span>（{b.reason}）{' '}
          {b.deletable ? (
            <button
              type="button"
              className="ghost"
              onClick={() => {
                if (window.confirm('壊れたアップロードを捨てますか？')) void upload.discardBroken(b.dir)
              }}
            >
              捨てる
            </button>
          ) : (
            '自動では消さない（管理者が確かめる）'
          )}
        </p>
      ))}
    </div>
  )
}

function DropZone({ upload, disabled, why }: { upload: UploadState; disabled: boolean; why: string }) {
  const [over, setOver] = useState(false)
  const files = useRef<HTMLInputElement>(null)
  const folder = useRef<HTMLInputElement>(null)
  // webkitdirectory は React の型に無いので属性で付ける
  useEffect(() => {
    folder.current?.setAttribute('webkitdirectory', '')
  }, [])
  const pick = (input: HTMLInputElement | null) => {
    if (input?.files == null) return
    upload.addFiles(Array.from(input.files).map((file) => ({ file })))
    input.value = ''
  }
  return (
    <div
      className={`upload-drop${over ? ' over' : ''}${disabled ? ' disabled' : ''}`}
      onDragOver={(e) => {
        if (disabled) return
        e.preventDefault()
        setOver(true)
      }}
      onDragLeave={() => setOver(false)}
      onDrop={(e) => {
        e.preventDefault()
        setOver(false)
        if (!disabled) upload.addDrop(e.dataTransfer)
      }}
    >
      <p>zip・音声ファイル・フォルダをここへドロップ</p>
      <p>
        <button type="button" disabled={disabled} onClick={() => files.current?.click()}>
          ファイルを選ぶ
        </button>{' '}
        <button type="button" disabled={disabled} onClick={() => folder.current?.click()}>
          フォルダを選ぶ
        </button>
      </p>
      <p className="muted small">FLAC / WAV / ALAC / AIFF / MP3 / AAC(m4a) / Opus、zip（中のフォルダもそのまま）</p>
      {disabled && <p className="muted small">{why}</p>}
      <input ref={files} type="file" multiple accept={ACCEPT} hidden onChange={(e) => pick(e.currentTarget)} />
      <input ref={folder} type="file" multiple hidden onChange={(e) => pick(e.currentTarget)} />
    </div>
  )
}

function Progress({ sent, total }: { sent: number; total: number }) {
  return <progress className="sync-progress-bar" value={total === 0 ? 1 : sent / total} max={1} aria-label="送信の進捗" />
}

/** ② の表。サーバの origin の順に、このタブで送ったものの進み具合を重ねる。まだ origin の無い送信は末尾 */
function SendTable({ upload, frozen }: { upload: UploadState; frozen: boolean }) {
  const { view, sends } = upload
  const byOrigin = new Map<string, SendItem>()
  for (const s of sends) if (s.originId != null) byOrigin.set(s.originId, s)
  const origins = (view?.origins ?? []).filter((o) => !o.removed)
  const pending = sends.filter((s) => s.originId == null || !origins.some((o) => o.id === s.originId))
  if (origins.length === 0 && pending.length === 0) return <p className="muted small">まだ送っていない</p>
  return (
    <table className="history-table upload-sends">
      <tbody>
        {origins.map((o) => (
          <OriginRow key={o.id} o={o} send={byOrigin.get(o.id)} upload={upload} frozen={frozen} />
        ))}
        {pending.map((s) => (
          <tr key={`s${s.n}`}>
            <td className="path">{s.path}</td>
            <td className="upload-bar">
              <Progress sent={s.sent} total={s.total} />
            </td>
            <td className="nowrap muted">
              {formatBytes(s.sent)} / {formatBytes(s.total)}
            </td>
            <td>
              <SendStatus s={s} upload={upload} />
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

function SendStatus({ s, upload }: { s: SendItem; upload: UploadState }) {
  if (s.state === 'error') {
    return (
      <>
        <span className="error">{s.error}</span>{' '}
        <button type="button" className="ghost" onClick={() => upload.resend(s.n)}>
          再送
        </button>
      </>
    )
  }
  return <span className="muted">{s.state === 'queued' ? '待ち' : s.state === 'sending' ? '送信中' : '送信済み'}</span>
}

function OriginRow({ o, send, upload, frozen }: { o: UploadOriginView; send: SendItem | undefined; upload: UploadState; frozen: boolean }) {
  // このタブで送っている・失敗したものは送信キューの状態を出す
  const local = send != null && send.state !== 'done' ? send : null
  const st = originState(o)
  const sent = local?.sent ?? o.received
  const rejected = o.zip?.rejected ?? []
  return (
    <>
      <tr>
        <td className="path">{o.client_path}</td>
        <td className="upload-bar">
          <Progress sent={sent} total={o.total} />
        </td>
        <td className="nowrap muted">{o.complete ? formatBytes(o.total) : `${formatBytes(sent)} / ${formatBytes(o.total)}`}</td>
        <td>
          {local != null ? (
            <SendStatus s={local} upload={upload} />
          ) : (
            <>
              <Pill tone={st.tone}>{st.label}</Pill>
              {st.action === 'unpack' && !frozen && (
                <>
                  {' '}
                  <button type="button" className="ghost" disabled={o.zip?.unpacking === true} onClick={() => void upload.retryUnpack(o.id)}>
                    展開し直す
                  </button>
                </>
              )}
              {st.action === 'remove' && !frozen && (
                <>
                  {' '}
                  <button
                    type="button"
                    className="ghost"
                    title={!o.complete && !o.damaged ? '同じファイルをドロップし直すと最初から送り直す' : undefined}
                    onClick={() => void upload.removeOrigin(o.id)}
                  >
                    外す
                  </button>
                </>
              )}
            </>
          )}
        </td>
      </tr>
      {rejected.map((r) => (
        <tr key={`${o.id}:${r.entry}`} className="upload-rejected">
          <td colSpan={4} className="muted small">
            取り込めなかった: {r.entry}（{r.reason}）
          </td>
        </tr>
      ))}
    </>
  )
}

/** ③ の件の表 */
function GroupTable({ v, upload }: { v: View; upload: UploadState }) {
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set())
  const locked = v.busy
  if (v.groups.length === 0 && v.ignored.length === 0) {
    return <p className="muted small">{upload.sending || v.busy ? '送信・展開が終わるとここに並ぶ' : '置ける曲が無い'}</p>
  }
  const toggleOpen = (key: string) =>
    setOpen((s) => {
      const n = new Set(s)
      if (n.has(key)) n.delete(key)
      else n.add(key)
      return n
    })
  const checked = v.groups.filter((g) => g.included).map((g) => g.key)
  return (
    <>
      <table className="history-table upload-groups">
        <thead>
          <tr>
            <th className="upload-check" />
            <th>件（アルバムアーティスト — アルバム）</th>
            <th>曲</th>
            <th>形式</th>
            <th>状態</th>
          </tr>
        </thead>
        <tbody>
          {v.groups.map((g) => (
            <Fragment key={g.key}>
              <tr>
                <td className="upload-check">
                  <input
                    type="checkbox"
                    aria-label={`${groupLabel(g)} を置く`}
                    checked={g.included}
                    disabled={locked || !g.selectable}
                    title={g.selectable ? undefined : '曲が 1 曲もチェックされていない'}
                    onChange={() => void upload.patch((cur) => toggleGroupIn(cur, g.key))}
                  />
                </td>
                <td>
                  <button type="button" className="link upload-tri" aria-expanded={open.has(g.key)} onClick={() => toggleOpen(g.key)}>
                    {open.has(g.key) ? '▾' : '▸'}
                  </button>{' '}
                  {g.album == null ? <i>{groupLabel(g)}</i> : groupLabel(g)}
                </td>
                <td className="nowrap">{g.track_count}</td>
                <td className="nowrap">{g.format}</td>
                <td>
                  <Pill tone={statusTone(g.status)}>{statusLabel(g.status)}</Pill>
                </td>
              </tr>
              {open.has(g.key) && <GroupDetail g={g} upload={upload} locked={locked} />}
            </Fragment>
          ))}
          {v.unassigned_companions.length > 0 && (
            <tr>
              <td />
              <td colSpan={4} className="muted small">
                どの件にも付けない同梱ファイル: {v.unassigned_companions.join(', ')}
              </td>
            </tr>
          )}
          {v.ignored.length > 0 && (
            <tr>
              <td />
              <td colSpan={4} className="muted small">
                取り込まない: {v.ignored.join(', ')}（音声でも同梱でもない）
              </td>
            </tr>
          )}
        </tbody>
      </table>
      <div className="op-row upload-ops">
        <button type="button" className="ghost" disabled={locked || checked.length < 2} onClick={() => void upload.merge(checked)}>
          選んだ件を 1 件にまとめる
        </button>
        <span className="muted small">（2 件以上をチェックしたとき。名前は先頭の件のもの。承認画面で直す）</span>
      </div>
    </>
  )
}

function MemberLine({
  m,
  isTrack,
  upload,
  locked,
}: {
  m: UploadMemberView
  isTrack: boolean
  upload: UploadState
  locked: boolean
}) {
  const name = m.display.split('/').pop() ?? m.display
  return (
    <li className={m.library_match != null ? 'upload-dup' : undefined}>
      <label>
        <input
          type="checkbox"
          checked={m.included}
          disabled={locked}
          onChange={() => void upload.patch((cur) => toggleFileIn(cur, m.staged))}
        />{' '}
        {isTrack ? '' : '同梱: '}
        {m.renamed ? `${name} → ${m.publish_name}` : name}
      </label>
      {m.library_match != null && <span className="small"> → ライブラリ: {m.library_match}</span>}
    </li>
  )
}

function GroupDetail({ g, upload, locked }: { g: UploadGroupView; upload: UploadState; locked: boolean }) {
  const more = g.statuses.slice(1)
  return (
    <tr className="upload-files">
      <td />
      <td colSpan={4}>
        {more.length > 0 && (
          <p className="small">
            {more.map((s, i) => (
              <Fragment key={i}>
                <Pill tone={statusTone(s)}>{statusLabel(s)}</Pill>{' '}
              </Fragment>
            ))}
          </p>
        )}
        <ul className="upload-members">
          {g.tracks.map((m) => (
            <MemberLine key={m.staged} m={m} isTrack upload={upload} locked={locked} />
          ))}
          {g.companions.map((m) => (
            <MemberLine key={m.staged} m={m} isTrack={false} upload={upload} locked={locked} />
          ))}
        </ul>
      </td>
    </tr>
  )
}

const STAGE_LABEL: Record<string, string> = {
  planned: '待ち',
  assembled: '用意済み',
  publishing: '置いている',
  published: '置いた',
}

/** 凍結した後の ③: 計画の件と進み具合（変更はできない） */
function FrozenPlan({ v }: { v: View }) {
  const groups = v.publish?.groups ?? []
  return (
    <table className="history-table upload-groups">
      <thead>
        <tr>
          <th>件（アルバムアーティスト — アルバム）</th>
          <th>曲</th>
          <th>置き場所</th>
          <th>状態</th>
        </tr>
      </thead>
      <tbody>
        {groups.map((g) => (
          <tr key={g.key}>
            <td>{g.album == null ? <i>アルバム名なし</i> : `${g.albumartist} — ${g.album}`}</td>
            <td className="nowrap">
              {g.track_count}
              {g.companion_count > 0 && <span className="muted small">（同梱 {g.companion_count}）</span>}
            </td>
            <td className="path">{'dest' in g ? g.dest : g.base_dest}</td>
            <td>
              <Pill tone={g.stage === 'published' ? 'ok' : 'info'}>{STAGE_LABEL[g.stage] ?? g.stage}</Pill>
            </td>
          </tr>
        ))}
      </tbody>
    </table>
  )
}

function PublishStep({ v, upload }: { v: View; upload: UploadState }) {
  const can = canPublish(v, upload.sending)
  const frozen = v.publish_requested_at != null || v.publish != null
  const running = publishing(v)
  const discard = () => {
    if (window.confirm('送ったものを捨てますか？（Inbox には置かない。ステージングは後で GC が消す）')) void upload.discard()
  }
  return (
    <>
      {v.publish_error != null && <p className="error small">置けなかった: {v.publish_error}</p>}
      <div className="op-row">
        <button
          type="button"
          className="primary"
          disabled={!can.ok || upload.mutating || running}
          onClick={() => void upload.publish()}
        >
          {v.publish_error != null ? 'もう一度置く' : running ? '置いている…' : publishLabel(v)}
        </button>
        {(!frozen || v.publish_error != null) && (
          <button type="button" className="ghost" disabled={!canDiscard(v) || upload.sending} onClick={discard}>
            送ったものを捨てる
          </button>
        )}
        {can.reason != null && <span className="muted small">{can.reason}</span>}
        {frozen && v.publish_error == null && !running && <span className="muted small">置き終えた</span>}
      </div>
    </>
  )
}
