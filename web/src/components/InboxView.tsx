// Inbox タブ（SPEC §12.6、D-68）。左に件の一覧、右に選んだ件の補正フォーム。
// 承認すると下書きがサーバに保存され、inbox ジョブが配置する。却下はファイルを Inbox に残したまま
// 一覧から外す（再開できる）。placed の件は 24 時間残るので、そこからアルバムへ飛べる

import { useMemo, useState } from 'react'
import type { InboxState } from '../hooks/useInbox'
import { formatDuration } from '../lib/format'
import { formatDateTime } from '../lib/history'
import {
  codecSummary,
  destinationLabel,
  draftForSubmit,
  draftFrom,
  isEditable,
  itemTitle,
  stateLabel,
  validateDraft,
  verdictLabel,
  type DraftTrack,
  type InboxDraft,
  type InboxItem,
  type InboxSource,
} from '../lib/inbox'
import { CategoryField } from './CdView'

export function InboxView({ inbox, onOpenAlbum }: { inbox: InboxState; onOpenAlbum: (albumId: number) => void }) {
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const items = inbox.items ?? NO_ITEMS
  // 選んでいた件が消えたら（placed の期限切れ・ディレクトリの消失）最初の件を出す
  const selected = items.find((i) => i.id === selectedId) ?? items[0] ?? null

  return (
    <section className="cd inbox">
      <div className="table-toolbar">
        <h1>Inbox</h1>
        <span className="spacer" />
        <button type="button" disabled={inbox.busy || inbox.unavailable} onClick={() => void inbox.scan()}>
          今すぐ確認
        </button>
      </div>
      <p className="muted small">
        Inbox のディレクトリに置いた音声ファイルをここで確かめてから配置する。件 = ディレクトリ。承認すると
        補正した内容をタグに書いて {'{category}'}/{'{albumartist}'}/{'{album}'} へ移し、Inbox からは消える
      </p>
      {inbox.notice != null && (
        <p className="small">
          {inbox.notice}{' '}
          <button type="button" className="ghost" onClick={() => inbox.setNotice(null)}>
            閉じる
          </button>
        </p>
      )}
      {inbox.error != null && <p className="error">{inbox.error}</p>}
      {inbox.items != null && items.length === 0 && !inbox.unavailable && (
        <p className="muted">件はない。ファイルを置くと定期の走査（設定 [inbox].poll_interval_secs）か「今すぐ確認」で現れる</p>
      )}
      {items.length > 0 && (
        <div className="inbox-body">
          <ul className="inbox-list">
            {items.map((it) => (
              <li key={it.id} className={it.id === selected?.id ? 'selected' : ''}>
                <button type="button" onClick={() => setSelectedId(it.id)}>
                  <span className="inbox-title">{itemTitle(it)}</span>
                  <span className={`badge inbox-state-${it.state}`}>{stateLabel(it.state)}</span>
                  <span className="muted small">
                    {it.tracks.length} ファイル · {codecSummary(it.tracks)} · 検出 {formatDateTime(it.detected_at)}
                  </span>
                  {unmatchedCount(it) > 0 && (
                    <span className="badge inbox-verdict-ng">未判定 {unmatchedCount(it)}</span>
                  )}
                  {it.error != null && <span className="error small">{it.error}</span>}
                </button>
              </li>
            ))}
          </ul>
          {selected != null && (
            <ItemForm key={selected.id} item={selected} inbox={inbox} onOpenAlbum={onOpenAlbum} />
          )}
        </div>
      )}
    </section>
  )
}

function ItemForm({
  item,
  inbox,
  onOpenAlbum,
}: {
  item: InboxItem
  inbox: InboxState
  onOpenAlbum: (albumId: number) => void
}) {
  // 初期値は保存済みの下書き（無ければ提案）。編集中は一覧が取り直されても上書きしない。
  // 走査でファイルが変わった（pending に戻った）ときだけ作り直す（描画中の setState で前回の鍵を持つ
  // React の作法。App.tsx の settleFilterTotal と同じ）
  const [draft, setDraft] = useState<InboxDraft>(() => draftFrom(item))
  const [fileKey, setFileKey] = useState(() => filesKey(item))
  const currentKey = filesKey(item)
  if (currentKey !== fileKey) {
    setFileKey(currentKey)
    setDraft(draftFrom(item))
  }
  const [submitError, setSubmitError] = useState<string | null>(null)

  const files = useMemo(() => item.tracks.map((f) => f.rel_path), [item.tracks])
  const byPath = useMemo(() => new Map(item.tracks.map((f) => [f.rel_path, f])), [item.tracks])
  const problems = useMemo(() => validateDraft(draft, files), [draft, files])
  const editable = isEditable(item.state)
  const placedAlbumId = item.placed_album_id
  // ダウンローダが置いた件（サイドカーあり）だけ判定の列を出す
  const hasSource = item.tracks.some((f) => f.source != null)

  const update = (patch: Partial<InboxDraft>) => setDraft((d) => ({ ...d, ...patch }))
  const updateTrack = (i: number, patch: Partial<DraftTrack>) =>
    setDraft((d) => ({ ...d, tracks: d.tracks.map((t, j) => (j === i ? { ...t, ...patch } : t)) }))
  const approve = async () => {
    setSubmitError(null)
    const msg = await inbox.approve(item.id, draftForSubmit(draft))
    if (msg != null) setSubmitError(msg)
  }
  const text = (label: string, key: 'albumartist' | 'album') => (
    <label className="cd-field">
      <span>{label}</span>
      <input type="text" value={draft[key]} disabled={!editable} onChange={(e) => update({ [key]: e.target.value })} />
    </label>
  )
  const num = (t: DraftTrack, i: number, key: 'disc_no' | 'track_no', label: string) => (
    <input
      type="number"
      min={1}
      max={999}
      aria-label={`${t.rel_path} の${label}`}
      value={t[key]}
      disabled={!editable}
      onChange={(e) => {
        const v = Number.parseInt(e.target.value, 10)
        updateTrack(i, { [key]: Number.isFinite(v) ? v : 0 })
      }}
    />
  )

  return (
    <div className="inbox-form">
      <h2>
        {itemTitle(item)} <span className={`badge inbox-state-${item.state}`}>{stateLabel(item.state)}</span>
      </h2>
      {item.state === 'placed' && placedAlbumId != null && (
        <div className="op-row">
          <button type="button" className="primary" onClick={() => onOpenAlbum(placedAlbumId)}>
            配置済み → アルバムを開く
          </button>
          {item.placed_at != null && <span className="muted small">{formatDateTime(item.placed_at)} に配置</span>}
        </div>
      )}
      {item.state === 'failed' && item.error != null && <p className="error">失敗: {item.error}</p>}
      {item.state === 'approved' && (
        <p className="muted small">
          {item.approved_at != null ? `${formatDateTime(item.approved_at)} に承認。` : ''}
          配置はジョブで進む。直すなら「下書きに戻す」
        </p>
      )}
      {item.warnings.length > 0 && (
        <ul className="cd-warnings small">
          {item.warnings.map((w) => (
            <li key={w}>{w}</li>
          ))}
        </ul>
      )}
      {destinationLabel(item.destination) != null && (
        <p className="notice small inbox-destination">{destinationLabel(item.destination)}</p>
      )}

      <div className="cd-form">
        {text('アルバムアーティスト', 'albumartist')}
        {text('アルバム', 'album')}
        <label className="cd-field">
          <span>日付（YYYY / YYYY-MM / YYYY-MM-DD。空なら書かない）</span>
          <input
            type="text"
            value={draft.date ?? ''}
            disabled={!editable}
            placeholder="YYYY-MM-DD"
            onChange={(e) => update({ date: e.target.value === '' ? null : e.target.value })}
          />
        </label>
        {editable ? (
          <CategoryField value={draft.category} onChange={(v) => update({ category: v })} />
        ) : (
          <label className="cd-field">
            <span>category</span>
            <input type="text" value={draft.category ?? '_Unsorted'} disabled />
          </label>
        )}
      </div>

      <h2>トラック（ファイル名・コーデック・長さはファイルから）</h2>
      <table className="cd-tracks cd-tracks-edit inbox-tracks">
        <thead>
          <tr>
            <th>disc</th>
            <th>#</th>
            <th>タイトル</th>
            <th>アーティスト（空ならアルバムアーティスト）</th>
            <th>ファイル</th>
            <th>長さ</th>
            {hasSource && <th>判定</th>}
          </tr>
        </thead>
        <tbody>
          {draft.tracks.map((t, i) => {
            const f = byPath.get(t.rel_path)
            return (
              <tr key={t.rel_path} className={f?.source != null && f.source.verdict !== 'ok' ? 'inbox-unmatched' : ''}>
                <td className="inbox-num">{num(t, i, 'disc_no', 'ディスク番号')}</td>
                <td className="inbox-num">{num(t, i, 'track_no', 'トラック番号')}</td>
                <td>
                  <input
                    type="text"
                    aria-label={`${t.rel_path} のタイトル`}
                    value={t.title}
                    disabled={!editable}
                    onChange={(e) => updateTrack(i, { title: e.target.value })}
                  />
                </td>
                <td>
                  <input
                    type="text"
                    aria-label={`${t.rel_path} のアーティスト`}
                    value={t.artist}
                    placeholder={draft.albumartist}
                    disabled={!editable}
                    onChange={(e) => updateTrack(i, { artist: e.target.value })}
                  />
                </td>
                <td className="muted small">
                  {fileName(t.rel_path)}
                  {f != null ? ` · ${f.codec}${f.lossless ? '' : '（非可逆）'}` : ''}
                </td>
                <td className="muted small">{f != null ? formatDuration(f.duration_ms) : ''}</td>
                {hasSource && <td className="inbox-verdict">{f?.source != null && <VerdictCell source={f.source} />}</td>}
              </tr>
            )
          })}
        </tbody>
      </table>
      {editable && problems.length > 0 && (
        <ul className="cd-errors small">
          {problems.map((p) => (
            <li key={p} className="error">
              {p}
            </li>
          ))}
        </ul>
      )}
      {submitError != null && <p className="error">{submitError}</p>}
      <div className="op-row">
        {editable && (
          <button type="button" className="primary" disabled={inbox.busy || problems.length > 0} onClick={() => void approve()}>
            承認して配置
          </button>
        )}
        {(item.state === 'pending' || item.state === 'failed' || item.state === 'approved') && (
          <button type="button" disabled={inbox.busy} onClick={() => void inbox.reject(item.id)}>
            却下
          </button>
        )}
        {(item.state === 'approved' || item.state === 'rejected' || item.state === 'failed') && (
          <button type="button" disabled={inbox.busy} onClick={() => void inbox.reopen(item.id)}>
            下書きに戻す
          </button>
        )}
      </div>
    </div>
  )
}

/** 判定バッジ。判定できなかったものは行を開くと message（ルールの足し方）と URL が読める */
function VerdictCell({ source }: { source: InboxSource }) {
  const v = verdictLabel(source)
  const badge = <span className={`badge ${v.ok ? 'inbox-verdict-ok' : 'inbox-verdict-ng'}`}>{v.text}</span>
  if (v.ok && source.url == null) return badge
  return (
    <details className="inbox-verdict-details">
      <summary>{badge}</summary>
      {source.message != null && <pre className="inbox-verdict-message small">{source.message}</pre>}
      {source.url != null && (
        <a className="small" href={source.url} target="_blank" rel="noreferrer">
          {source.url}
        </a>
      )}
      {source.channel != null && <div className="muted small">channel: {source.channel}</div>}
    </details>
  )
}

const NO_ITEMS: InboxItem[] = []

/** 判定できなかったトラックの数（一覧のバッジ） */
function unmatchedCount(item: InboxItem): number {
  return item.tracks.filter((f) => f.source != null && f.source.verdict !== 'ok').length
}

/** 件のファイル集合の鍵（走査で変わったかの判定に使う） */
function filesKey(item: InboxItem): string {
  return item.tracks.map((f) => `${f.rel_path}\u0000${f.inode}\u0000${f.mtime_ns}\u0000${f.size}`).join('\n')
}

function fileName(relPath: string): string {
  const i = relPath.lastIndexOf('/')
  return i < 0 ? relPath : relPath.slice(i + 1)
}
