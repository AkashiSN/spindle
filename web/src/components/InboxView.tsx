// Inbox タブ（SPEC §12.6、D-68 / D-86）。左に件の一覧（埋め込み画像のサムネイル付き）、右に選んだ件の承認画面。
// 承認画面は CD 画面と同じく番号付きの段で流れを見せる: ① 取り込む件 → ② アルバム情報（画像の欄と、
// ライブラリのプロパティと同じ操作の表）→ ③ トラック（全項目をダブルクリックで編集）→ ④ 確認して配置
// （確認項目・配置先の見込み・承認）。下書きは画面の中だけで持ち、承認でサーバに保存して inbox ジョブが
// 配置する。却下はファイルを Inbox に残したまま件を却下にする（再開できる）。却下した件の「削除」は
// 破棄待ちにするだけで、ファイルは GC が retention 日後に消す（D-90）。placed の件は 24 時間残るので、
// そこからアルバムへ飛べる

import { useEffect, useMemo, useState } from 'react'
import { apiPost } from '../api/client'
import { useArtworkUpload } from '../hooks/useArtworkUpload'
import type { InboxState } from '../hooks/useInbox'
import { useInboxPreview } from '../hooks/useInboxPreview'
import { describeLookupError } from '../hooks/useCdLookup'
import {
  candidateDetail,
  lookupHeadline,
  matchedByLabel,
  releaseUrl,
  type LookupResponse,
  type ReleaseCandidate,
} from '../lib/cd'
import { formatDateTime } from '../lib/history'
import {
  applyCandidate,
  applyTracklist,
  artworkUrl,
  codecSummary,
  destinationText,
  discardLabel,
  discNumbers,
  draftChangeCount,
  draftForSubmit,
  draftFrom,
  isEditable,
  itemCover,
  itemTitle,
  pictureState,
  sameTitleCount,
  stateLabel,
  syncNote,
  trackPictureUrl,
  validateDraft,
  watchLabel,
  type InboxDraft,
  type InboxFile,
  type InboxItem,
  type RipLookup,
} from '../lib/inbox'
import { parseTracklist } from '../lib/tracklist'
import { InboxAlbumProps } from './InboxAlbumProps'
import { InboxCover } from './InboxCover'
import { InboxTrackGrid } from './InboxTrackGrid'
import { Step } from './Step'

export function InboxView({
  inbox,
  onOpenAlbum,
  focusDir = null,
  onFocused,
}: {
  inbox: InboxState
  onOpenAlbum: (albumId: number) => void
  /** 開いたときに選ぶ件のディレクトリ（YouTube 画面の ④ から。D-87）。選んだら（無くても）onFocused */
  focusDir?: string | null
  onFocused?: () => void
}) {
  const [selectedId, setSelectedId] = useState<number | null>(null)
  const items = inbox.items ?? NO_ITEMS
  // 指された件が一覧に現れたら選ぶ（ytdl の直後は Inbox の検出がまだで、後から SSE で現れることがある）。
  // 選んだら親に知らせて focusDir を消してもらう。現れない間は待ち続け、人が別の件を選んだらやめる
  const focusHit = focusDir != null ? items.find((i) => i.rel_dir === focusDir) : undefined
  if (focusHit != null && selectedId !== focusHit.id) setSelectedId(focusHit.id)
  useEffect(() => {
    if (focusHit != null && selectedId === focusHit.id) onFocused?.()
  }, [focusHit, selectedId, onFocused])
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
                <button
                  type="button"
                  onClick={() => {
                    setSelectedId(it.id)
                    onFocused?.()
                  }}
                >
                  <ItemThumb item={it} />
                  <span className="inbox-item-text">
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
                  {it.state === 'rejected' && it.discard_requested_at != null && (
                    <span className="badge inbox-discard-badge">削除待ち</span>
                  )}
                  {it.error != null && <span className="error small">{it.error}</span>}
                  </span>
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

  const names = useMemo(() => item.tracks.map((f) => f.rel_path), [item.tracks])
  const files = useMemo(() => new Map(item.tracks.map((f) => [f.rel_path, f])), [item.tracks])
  const problems = useMemo(() => validateDraft(draft, names), [draft, names])
  const editable = isEditable(item.state)
  const placedAlbumId = item.placed_album_id
  const artwork = useArtworkUpload()
  const preview = useInboxPreview(item.id, draft, editable)
  const pics = pictureState(files, draft)
  const changes = draftChangeCount(item, draft)
  const source = item.rip != null ? 'CD' : item.tracks.some((f) => f.source != null) ? 'YouTube' : '手置き'
  const cover = draftCoverUrl(item, draft, files)
  const dest = destinationText(item, draft)
  const sync = syncNote(item, draft, inbox.subscriptions, formatDateTime)

  const approve = async () => {
    setSubmitError(null)
    const msg = await inbox.approve(item.id, draftForSubmit(draft))
    if (msg != null) setSubmitError(msg)
  }

  // ④ の確認項目: 赤（配置できない）/ 黄（注意だけ）/ 緑（配置で行うこと）
  const warns: string[] = []
  const unmatched = unmatchedCount(item)
  if (unmatched > 0) warns.push(`判定できなかった曲が ${unmatched} 曲（③ の判定列を開くとルールの足し方が読める）`)
  const same = sameTitleCount(item)
  if (same > 0) warns.push(`Library に同名の曲がある: ${same} 曲（③ のタイトルの ⚠。別テイクなら承認してよい）`)
  if (pics.mode === 'none') warns.push('カバー画像なし（② で追加できる。配置後に操作タブの「アートワーク」でも足せる）')
  else if (pics.missing > 0) warns.push(`画像の無い曲が ${pics.missing} 曲（② の「画像の無い曲に入れる」か ③ の画像列で足せる）`)
  const tagTracks = draft.tracks.filter((t) => Object.keys(t.tags ?? {}).length > 0).length
  const oks: string[] = []
  if (tagTracks > 0) oks.push(`${tagTracks} 曲のファイルのタグを直して書く`)
  if (pics.changed > 0) oks.push(`${pics.changed} 曲の画像を埋め込む / 差し替える（ほかの曲の画像はそのまま）`)

  return (
    <div className="inbox-form">
      <div className="inbox-head">
        {cover != null ? (
          <img className="inbox-cover" src={cover} alt="" title="件の代表画像（配置後のアルバムの代表とは限らない）" />
        ) : (
          <span className="inbox-cover empty" aria-hidden="true" />
        )}
        <div>
          <h2>{draft.album.trim() || itemTitle(item)}</h2>
          <div className="inbox-badges">
            <span className="badge">{source}</span>
            <span className={`badge inbox-state-${item.state}`}>{stateLabel(item.state)}</span>
            {changes > 0 && <span className="badge inbox-changed-badge">変更 {changes} 件</span>}
          </div>
        </div>
      </div>
      {item.state === 'placed' && placedAlbumId != null && (
        <div className="op-row">
          <button type="button" className="primary" onClick={() => onOpenAlbum(placedAlbumId)}>
            配置済み → アルバムを開く
          </button>
          {item.placed_at != null && <span className="muted small">{formatDateTime(item.placed_at)} に配置</span>}
        </div>
      )}
      {item.state === 'failed' && item.error != null && <p className="error">失敗: {item.error}</p>}
      {item.state === 'rejected' && item.error != null && <p className="muted small">{item.error}</p>}
      {discardLabel(item, inbox.discardRetentionDays, formatDateTime) != null && (
        <p className="notice small">{discardLabel(item, inbox.discardRetentionDays, formatDateTime)}</p>
      )}
      {item.state === 'approved' && (
        <p className="muted small">
          {item.approved_at != null ? `${formatDateTime(item.approved_at)} に承認。` : ''}
          配置はジョブで進む。直すなら「下書きに戻す」
        </p>
      )}

      <Step
        no={1}
        title="取り込む件"
        done
        aside={
          <span className="muted">
            {item.tracks.length} ファイル · {codecSummary(item.tracks)}
          </span>
        }
        hint="Inbox のディレクトリと、そこから読んだこと。出どころ（CD / YouTube / 手置き）で ③ の列と ④ の確認項目が変わる"
      >
        <dl className="cd-summary">
          <div>
            <dt>出どころ</dt>
            <dd>{source}</dd>
          </div>
          <div>
            <dt>ディレクトリ</dt>
            <dd>
              <code>Inbox/{item.rel_dir}</code>
            </dd>
          </div>
          <div>
            <dt>検出</dt>
            <dd>{formatDateTime(item.detected_at)}</dd>
          </div>
        </dl>
        {dest != null && <p className="notice small inbox-destination">{dest.label}</p>}
        {dest?.overlap != null && <p className="error small">{dest.overlap}</p>}
        {sync != null && <p className="muted small inbox-sync-note">{sync}</p>}
        {item.warnings.length > 0 && (
          <>
            <p className="muted small inbox-file-notes-head">ファイルのタグで足りないもの（② / ③ で補う）</p>
            <ul className="cd-warnings small">
              {item.warnings.map((w) => (
                <li key={w}>{w}</li>
              ))}
            </ul>
          </>
        )}
      </Step>

      <Step
        no={2}
        title="アルバム情報"
        aside={editable ? <span className="muted">ダブルクリックで編集</span> : undefined}
        hint={
          <>
            ライブラリのプロパティと同じ操作。行をクリックで選び、ダブルクリック（Enter / F2）で入力欄になる。Enter で
            確定、Esc で取り消し。「変更」はファイルの値から直したもの（行にカーソルを置くと元の値）。category は語彙から
            選ぶ、album gain はダブルクリックで切り替わる。画像は配置のときに埋め込む
          </>
        }
      >
        <InboxCover item={item} draft={draft} files={files} editable={editable} artwork={artwork} update={setDraft} />
        <InboxAlbumProps item={item} draft={draft} editable={editable} onChange={(p) => setDraft((d) => ({ ...d, ...p }))} />
        {editable && item.rip != null && <MbLookup rip={item.rip} draft={draft} onApply={setDraft} />}
      </Step>

      <Step
        no={3}
        title="トラック"
        aside={
          <span className="muted">
            {draft.tracks.length} 曲{editable ? ' · セルをダブルクリックで編集' : ''}
          </span>
        }
        hint={
          <>
            セルをクリックで選び、ダブルクリック（Enter / F2）で編集。↑↓←→ で直せるセルを移る。アルバム / アルバム
            アーティスト / 日付はアルバム単位で、どの行で直しても全行と ② に反映する。タグの列は空にするとそのタグを
            消し、「タグを追加」で新しいキーの列を足せる。画像の列はその曲だけ差し替える。長さ・codec・ファイル名と 🔒
            の列は直せない。左の 4 列は横にスクロールしても残る
          </>
        }
      >
        <InboxTrackGrid item={item} draft={draft} files={files} editable={editable} artwork={artwork} update={setDraft} />
        {editable && <TracklistPaste draft={draft} onApply={setDraft} />}
      </Step>

      <Step
        no={4}
        title="確認して配置"
        hint="赤の項目があると配置できない。黄の項目は注意だけで、承認は止めない。承認すると inbox ジョブがタグと画像を書いて配置先へ移す"
      >
        <ul className="inbox-checks small">
          {editable && problems.map((p) => <li key={p} className="ng">{p}</li>)}
          {warns.map((w) => (
            <li key={w} className="wa">
              {w}
            </li>
          ))}
          {oks.map((o) => (
            <li key={o} className="ok">
              {o}
            </li>
          ))}
          {editable && problems.length === 0 && warns.length === 0 && oks.length === 0 && <li className="ok">問題なし</li>}
        </ul>
        {editable && (
          <div className="inbox-dest">
            <span className="muted small">配置先（見込み）</span>
            <code>
              {preview == null
                ? '…'
                : preview.rel_dir != null
                  ? `Library/${preview.rel_dir}/`
                  : `決められない: ${preview.error ?? ''}`}
            </code>
          </div>
        )}
        {submitError != null && <p className="error">{submitError}</p>}
        <div className="op-row">
          {editable && (
            <button
              type="button"
              className="primary"
              disabled={inbox.busy || artwork.busy || problems.length > 0}
              onClick={() => void approve()}
            >
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
          {item.state === 'rejected' && item.discard_requested_at == null && (
            <button
              type="button"
              className="danger"
              disabled={inbox.busy}
              onClick={() => {
                const days = inbox.discardRetentionDays
                const when = days == null ? '期限が来たら' : `${days} 日後の`
                if (
                  window.confirm(
                    `Inbox/${item.rel_dir} のファイル（${item.tracks.length} 曲と同梱の画像など）を削除しますか？\n` +
                      `すぐには消さず、${when} GC が消す。それまでは「削除を取り消す」で戻せる`,
                  )
                )
                  void inbox.discard(item.id)
              }}
            >
              削除
            </button>
          )}
          {item.state === 'rejected' && item.discard_requested_at != null && (
            <button type="button" disabled={inbox.busy} onClick={() => void inbox.undiscard(item.id)}>
              削除を取り消す
            </button>
          )}
          {editable && (
            <span className="muted small">
              {problems.length > 0 ? '赤の項目を直すと押せる' : `${draft.tracks.length} 曲を配置する`}
            </span>
          )}
        </div>
      </Step>
    </div>
  )
}

/** 見出しの画像: 下書きを当てた後の各曲の画像の最頻（同数なら先の曲） */
function draftCoverUrl(item: InboxItem, draft: InboxDraft, files: ReadonlyMap<string, InboxFile>): string | null {
  const counts = new Map<string, number>()
  for (const t of draft.tracks) {
    const u = trackPictureUrl(item.id, files.get(t.rel_path), t)
    if (u != null) counts.set(u, (counts.get(u) ?? 0) + 1)
  }
  let best: string | null = null
  let max = 0
  for (const [u, n] of counts) {
    if (n > max) {
      best = u
      max = n
    }
  }
  return best
}

/**
 * CD の件の MusicBrainz 引き直し（P4-21）。候補が無いまま / 「どれも違う」で取り込んだ盤を、後から
 * （DiscID を登録した・リリースを見つけた）承認画面で引き直す。照会は CD 画面と同じ `POST /api/cd/lookup`
 * （サイドカーの TOC / ISRC / MCN。10 分のキャッシュと 1 req/s はサーバ側）。選んだ候補は ID を写し、
 * 空欄の名前だけ埋める（`applyCandidate`）。結果は保存しない（件を替えると消える）
 */
function MbLookup({ rip, draft, onApply }: { rip: RipLookup; draft: InboxDraft; onApply: (d: InboxDraft) => void }) {
  const [release, setRelease] = useState('')
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [result, setResult] = useState<LookupResponse | null>(null)
  const run = async (opts: { refresh?: boolean; widen?: boolean } = {}) => {
    setBusy(true)
    setError(null)
    try {
      setResult(
        await apiPost<LookupResponse>('/api/cd/lookup', {
          toc: rip.toc,
          isrcs: rip.isrcs,
          mcn: rip.mcn,
          release: release.trim() === '' ? null : release.trim(),
          refresh: opts.refresh ?? false,
          widen: opts.widen ?? false,
        }),
      )
    } catch (e) {
      setError(describeLookupError(e))
    } finally {
      setBusy(false)
    }
  }
  const chosen = draft.release_id ?? null
  const pick = (c: ReleaseCandidate) => onApply(applyCandidate(draft, c))
  // 最初は「未選択なら開く」。以後の開閉は利用者の操作を state に持つ（再描画で閉じ直さない）
  const [open, setOpen] = useState(chosen == null)
  return (
    <details className="inbox-mb" open={open} onToggle={(e) => setOpen(e.currentTarget.open)}>
      <summary className="small">
        MusicBrainz{' '}
        {chosen == null ? (
          <span className="muted">（リリース未選択）</span>
        ) : (
          <a href={`https://musicbrainz.org/release/${chosen}`} target="_blank" rel="noopener noreferrer">
            {chosen}
          </a>
        )}
      </summary>
      <p className="muted small">
        CD 画面で候補が無かった盤を引き直す（DiscID を登録した後など）。選ぶとリリースの ID を写し、空欄の
        名前（と Track NN）だけ埋める。手で入れた値は変えない
      </p>
      <div className="op-row">
        <button type="button" disabled={busy} onClick={() => void run({ refresh: result != null })}>
          {busy ? '照会中…' : 'MusicBrainz で引き直す'}
        </button>
        <input
          type="text"
          className="small"
          aria-label="リリース URL / MBID（任意）"
          placeholder="リリース URL / MBID（任意）"
          value={release}
          onChange={(e) => setRelease(e.target.value)}
        />
      </div>
      {error != null && <p className="error small">{error}</p>}
      {result != null && (
        <>
          <p className="small">{lookupHeadline(result)}</p>
          {result.notes.map((n) => (
            <p key={n} className="error small">
              {n}
            </p>
          ))}
          <ul className="inbox-mb-candidates">
            {result.candidates.map((c) => (
              <li key={`${c.release_id}-${c.medium_position}`}>
                <label>
                  <input
                    type="radio"
                    name="inbox-mb-candidate"
                    checked={chosen === c.release_id && draft.tracks[0]?.disc_no === c.medium_position}
                    onChange={() => pick(c)}
                  />{' '}
                  <strong>{c.artist}</strong> — {c.title}{' '}
                  <span className={c.exact ? 'badge' : 'badge muted'}>{matchedByLabel(c)}</span>{' '}
                  <a href={releaseUrl(c)} target="_blank" rel="noopener noreferrer" className="small">
                    MusicBrainz で見る
                  </a>
                  <div className="muted small">{candidateDetail(c, result.tracks)}</div>
                </label>
              </li>
            ))}
          </ul>
          {result.can_widen && (
            <button type="button" disabled={busy} onClick={() => void run({ widen: true })}>
              さらに広げて探す
            </button>
          )}
        </>
      )}
    </details>
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

const NO_ITEMS: InboxItem[] = []

/** 一覧の件のサムネイル（件の代表の埋め込み画像）。無い件・読めない件は同じ寸法の空枠 */
function ItemThumb({ item }: { item: InboxItem }) {
  const cover = itemCover(item)
  const [failed, setFailed] = useState<string | null>(null)
  if (cover == null || failed === cover) return <span className="inbox-list-thumb empty" aria-hidden="true" />
  return (
    <img
      className="inbox-list-thumb"
      src={artworkUrl(item.id, cover)}
      alt=""
      loading="lazy"
      onError={() => setFailed(cover)}
    />
  )
}

/** 判定できなかったトラックの数（一覧のバッジ） */
function unmatchedCount(item: InboxItem): number {
  return item.tracks.filter((f) => f.source != null && f.source.verdict !== 'ok').length
}

/** 件のファイル集合の鍵（走査で変わったかの判定に使う） */
function filesKey(item: InboxItem): string {
  return item.tracks.map((f) => `${f.rel_path}\u0000${f.inode}\u0000${f.mtime_ns}\u0000${f.size}`).join('\n')
}
