// Inbox タブ（SPEC §12.6、D-68）。左に件の一覧、右に選んだ件の補正フォーム。
// 承認すると下書きがサーバに保存され、inbox ジョブが配置する。却下はファイルを Inbox に残したまま
// 一覧から外す（再開できる）。placed の件は 24 時間残るので、そこからアルバムへ飛べる

import { useMemo, useState } from 'react'
import type { InboxState } from '../hooks/useInbox'
import { formatDuration } from '../lib/format'
import { formatDateTime } from '../lib/history'
import {
  ARTIST_JOIN,
  applyTracklist,
  artistValues,
  artworkUrl,
  codecSummary,
  destinationLabel,
  discNumbers,
  draftForSubmit,
  draftFrom,
  isEditable,
  itemCover,
  itemTitle,
  pictureOf,
  stateLabel,
  validateDraft,
  sameTitleCount,
  sameTitleLabel,
  verdictLabel,
  watchLabel,
  type DraftTrack,
  type InboxDraft,
  type InboxItem,
  type InboxSource,
} from '../lib/inbox'
import { parseTracklist } from '../lib/tracklist'
import { CategoryField } from './CategoryField'

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
        <span className="muted small">{watchLabel(inbox.watch, formatDateTime)}</span>
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
        <p className="muted">件はない。ファイルを置くと定期の確認（設定 [inbox].poll_interval_secs。変化があったときだけ走査する）か「今すぐ確認」で現れる</p>
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
                  {sameTitleCount(it) > 0 && (
                    <span className="badge inbox-same-title-badge">同名 {sameTitleCount(it)}</span>
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
  // 埋め込み画像のあるファイルがあればサムネイル列を出す（P4-4）
  const hasPicture = item.tracks.some((f) => pictureOf(f) != null)
  const cover = itemCover(item)

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
        {cover != null && (
          <img
            className="inbox-cover"
            src={artworkUrl(item.id, cover)}
            alt=""
            title="件の埋め込み画像（多数派の目安。配置後のアルバムの代表画像とは限らない）"
          />
        )}
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
        <label className="cd-field inbox-album-gain">
          <span>album gain</span>
          <span className="small">
            <input
              type="checkbox"
              checked={draft.album_gain}
              disabled={!editable}
              onChange={(e) => update({ album_gain: e.target.checked })}
            />{' '}
            album gain を計算する（アルバム通し再生用。既定 off。CD 取り込みは on。追記先があればその現在値）
          </span>
        </label>
      </div>

      <h2>トラック（ファイル名・コーデック・長さはファイルから）</h2>
      <table className="cd-tracks cd-tracks-edit inbox-tracks">
        <thead>
          <tr>
            <th>disc</th>
            <th>#</th>
            {hasPicture && <th />}
            <th>タイトル</th>
            <th title="多値のファイルは元の値をチップで示す。「そのまま保つ」を外すと 1 値で書く">
              アーティスト（空ならアルバムアーティスト）
            </th>
            <th>ファイル</th>
            <th>長さ</th>
            {hasSource && <th>判定</th>}
          </tr>
        </thead>
        <tbody>
          {draft.tracks.map((t, i) => {
            const f = byPath.get(t.rel_path)
            const pic = f == null ? null : pictureOf(f)
            return (
              <tr key={t.rel_path} className={f?.source != null && f.source.verdict !== 'ok' ? 'inbox-unmatched' : ''}>
                <td className="inbox-num">{num(t, i, 'disc_no', 'ディスク番号')}</td>
                <td className="inbox-num">{num(t, i, 'track_no', 'トラック番号')}</td>
                {hasPicture && (
                  <td className="inbox-thumb-cell">
                    {pic != null && <img className="inbox-thumb" src={artworkUrl(item.id, pic)} alt="" />}
                  </td>
                )}
                <td>
                  <input
                    type="text"
                    aria-label={`${t.rel_path} のタイトル`}
                    value={t.title}
                    disabled={!editable}
                    onChange={(e) => updateTrack(i, { title: e.target.value })}
                  />
                  {f != null && sameTitleLabel(f) != null && (
                    <div className="inbox-same-title small" title="同じ曲を二重に取り込もうとしている可能性がある（Cover / Live ver. は別曲。承認は止めない）">
                      ⚠ {sameTitleLabel(f)}
                    </div>
                  )}
                </td>
                <td>
                  <ArtistCell
                    track={t}
                    values={f == null ? [] : artistValues(f)}
                    albumartist={draft.albumartist}
                    editable={editable}
                    onChange={(patch) => updateTrack(i, patch)}
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
      {editable && <TracklistPaste draft={draft} onApply={setDraft} />}
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

/**
 * トラックリスト貼り付け（P2-10、D-65。CD 画面から移した）。通販ページ等のテキストを行解析して
 * 選んだディスクの行へトラック番号で写す。写した後もフォームで直せる。本文は件を替えると消える
 */
function TracklistPaste({ draft, onApply }: { draft: InboxDraft; onApply: (d: InboxDraft) => void }) {
  const [text, setText] = useState('')
  const [artistFirst, setArtistFirst] = useState(false)
  const [warnings, setWarnings] = useState<string[]>([])
  const discs = discNumbers(draft)
  const [picked, setPicked] = useState<number | null>(null)
  // 選んでいたディスクが下書きから消えたら先頭へ
  const disc = picked != null && discs.includes(picked) ? picked : (discs[0] ?? 1)
  const apply = () => {
    const parsed = parseTracklist(text, { artistFirst })
    const r = applyTracklist(draft, disc, parsed.tracks)
    onApply(r.draft)
    setWarnings([...parsed.warnings, ...r.warnings])
  }
  return (
    <details className="inbox-paste">
      <summary className="small">トラックリスト貼り付け</summary>
      <p className="muted small">
        通販ページ等のテキストを 1 行 1 曲で貼る。行頭の番号（<code>1.</code> <code>01</code> <code>M-1</code>）と
        行末の時間は外し、<code>タイトル / アーティスト</code>（<code>／</code> <code>|</code> <code>-</code> も）で
        分ける。表（タブ区切り）も可。トラック番号で行に写すので、番号が無ければ上から 1, 2, …
      </p>
      <textarea
        aria-label="トラックリスト"
        rows={6}
        value={text}
        placeholder={'1. タイトル / アーティスト 4:32\n2. …'}
        onChange={(e) => setText(e.target.value)}
      />
      <div className="op-row">
        <button type="button" disabled={text.trim() === ''} onClick={apply}>
          行に写す
        </button>
        {discs.length > 1 && (
          <label className="small">
            写す先{' '}
            <select value={disc} onChange={(e) => setPicked(Number(e.target.value))}>
              {discs.map((n) => (
                <option key={n} value={n}>
                  ディスク {n}
                </option>
              ))}
            </select>
          </label>
        )}
        <label className="small">
          <input type="checkbox" checked={artistFirst} onChange={(e) => setArtistFirst(e.target.checked)} />{' '}
          アーティスト / タイトル の順で書かれている
        </label>
      </div>
      {warnings.length > 0 && (
        <ul className="cd-warnings small">
          {warnings.map((w) => (
            <li key={w}>{w}</li>
          ))}
        </ul>
      )}
    </details>
  )
}

/**
 * アーティスト欄（P4-4、D-70）。ファイルの ARTIST が多値なら元の値をチップで見せ、「そのまま保つ」
 * （提案は on。on の間は "; " 結合の表示で編集不可）を外すと欄が編集できて 1 値で書く
 */
function ArtistCell({
  track,
  values,
  albumartist,
  editable,
  onChange,
}: {
  track: DraftTrack
  values: string[]
  albumartist: string
  editable: boolean
  onChange: (patch: Partial<DraftTrack>) => void
}) {
  const multi = values.length > 1
  const keep = multi && track.keep_artists === true
  const input = (
    <input
      type="text"
      aria-label={`${track.rel_path} のアーティスト`}
      value={keep ? values.join(ARTIST_JOIN) : track.artist}
      placeholder={albumartist}
      disabled={!editable || keep}
      title={keep ? 'ファイルの多値をそのまま保つ（配置で ARTIST に触れない）' : undefined}
      onChange={(e) => onChange({ artist: e.target.value })}
    />
  )
  if (!multi) return input
  return (
    <div className="inbox-artist">
      {input}
      <div className="inbox-chips small">
        {values.map((v, i) => (
          <span key={`${i}-${v}`} className="chip">
            {v}
          </span>
        ))}
        <label className="inbox-keep">
          <input
            type="checkbox"
            checked={keep}
            disabled={!editable}
            // on / off とも欄は現在のファイルの結合値から始める（外した直後の欄 = 見えていた文字列）
            onChange={(e) => onChange({ keep_artists: e.target.checked, artist: values.join(ARTIST_JOIN) })}
          />{' '}
          そのまま保つ
        </label>
        {!keep && <span className="muted">1 値『{track.artist.trim() === '' ? albumartist : track.artist.trim()}』で書く</span>}
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
