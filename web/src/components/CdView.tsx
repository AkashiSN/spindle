// CD 画面（SPEC §12.6 のウィザード。P2-3 の「候補選択」と P2-4 の「手入力 / トラックリスト貼り付け」まで）。
// ドライブに CD が入っている前提の画面: 検出（P2-1）→ 自動で照会 → 候補を選ぶとフォームに写る。
// 候補は MusicBrainz へのリンク・収録構成（DVD 付き / BD 付き / デジタルの別）・ディスクとの長さ差で見分ける。
// CD 以外の medium に当たった候補は既定で畳む。TOC の貼り付けと各種 ID は「詳細」の中（ドライブ無しの環境と
// デバッグ用）。候補ゼロ件でも空のフォームで完走できる（D-21）。吸い出し（P2-5）は後続

import { Fragment, useState } from 'react'
import { useCategories } from '../hooks/useCategories'
import type { CdDriveState } from '../hooks/useCdDrive'
import type { CdLookupState } from '../hooks/useCdLookup'
import { driveIdsFor, driveStateLabel } from '../lib/cdDrive'
import {
  albumTags,
  candidateLengthMs,
  candidateSummary,
  COPY_SCOPE_LABELS,
  discidSubmissionUrl,
  formatLengthDiff,
  lengthDiffMs,
  lookupHeadline,
  matchedByLabel,
  mediaSummary,
  offersDiscidSubmission,
  releaseUrl,
  splitByMedium,
  trackTags,
  type CopyScope,
  type DiscMetadata,
  type ReleaseCandidate,
} from '../lib/cd'
import { formatDuration } from '../lib/format'

/** 候補の 2 行目: 収録構成・日付・国・レーベル・JAN/UPC・曲数・長さ・ディスクとの長さ差 */
function candidateDetail(c: ReleaseCandidate, tocTracks: { number: number; length_ms: number }[]): string {
  const parts = [mediaSummary(c), candidateSummary(c), `${c.tracks.length} 曲`]
  const len = candidateLengthMs(c)
  if (len != null) parts.push(formatDuration(len))
  const diff = lengthDiffMs(c, tocTracks)
  if (diff != null) parts.push(formatLengthDiff(diff))
  return parts.filter((p) => p !== '').join(' · ')
}

export function CdView({ cd, drive }: { cd: CdLookupState; drive: CdDriveState }) {
  const { result, draft, confirmed } = cd
  const driveLabel = drive.unavailable ? null : driveStateLabel(drive.status)
  // ドライブから読めた ISRC / MCN は、欄の TOC がその盤のものであるときだけ照会に添える
  // （別の盤の TOC を貼ったときに混ぜない。lib/cdDrive.ts の driveIdsFor）
  const ids = driveIdsFor(cd.toc, drive.status)
  const [releaseRef, setReleaseRef] = useState('')
  const [showOther, setShowOther] = useState(false)
  const canEject =
    drive.status != null && drive.status.state !== 'no_drive' && drive.status.state !== 'unknown' && !drive.ejecting
  const hasDisc = drive.status?.state === 'disc_ok'
  const split = splitByMedium(result?.candidates ?? [])
  // 候補の番号は元の一覧のもの（選択は index で持つ）
  const indexOf = (c: ReleaseCandidate) => (result?.candidates ?? []).indexOf(c)
  return (
    <section className="cd">
      <div className="table-toolbar">
        <h1>CD</h1>
        <span className="spacer" />
        {result != null && (
          <button type="button" className="ghost" onClick={cd.reset}>
            結果を消す
          </button>
        )}
      </div>

      <h2>ドライブ</h2>
      <div className="op-row">
        <span className="cd-drive-state" aria-live="polite">
          {drive.unavailable ? 'CD ドライブが使えない（コンテナにデバイスが渡っていない）' : (driveLabel ?? '…')}
        </span>
        {!drive.unavailable && (
          <>
            <button
              type="button"
              className="primary"
              disabled={cd.busy || !hasDisc || drive.status?.toc == null}
              onClick={() => void cd.lookupToc(drive.status?.toc ?? cd.toc, { ...ids, refresh: true })}
            >
              {cd.busy ? '照会中…' : '照会し直す'}
            </button>
            <button type="button" className="ghost" disabled={!canEject} onClick={() => void drive.eject()}>
              {drive.ejecting ? '取り出し中…' : '取り出す'}
            </button>
          </>
        )}
      </div>
      {!drive.unavailable && !hasDisc && (
        <p className="muted small">CD を入れると自動で読み取り、MusicBrainz に照会する</p>
      )}
      {hasDisc && (
        <p className="muted small">
          DiscID → 無ければ TOC 近似 + ディスクの{' '}
          <abbr title="International Standard Recording Code。録音ごとの国際コード。ディスクのサブチャネルに入っている">
            ISRC
          </abbr>{' '}
          /{' '}
          <abbr title="Japanese Article Number / Universal Product Code。商品のバーコード。ディスクに入っていれば読む">
            JAN/UPC
          </abbr>
          で引く。結果は 10 分覚えていて、「照会し直す」で引き直す
          {ids.isrcs.some((i) => i != null) ? `。ISRC: ${ids.isrcs.filter((i) => i != null).join(', ')}` : ''}
          {ids.mcn != null ? `。JAN/UPC: ${ids.mcn}` : ''}
        </p>
      )}
      {drive.error != null && <p className="error">{drive.error}</p>}

      {cd.error != null && <p className="error">{cd.error}</p>}

      {result != null && (
        <>
          <h2>{lookupHeadline(result)}</h2>
          {result.notes.map((n) => (
            <p key={n} className="error">
              {n}
            </p>
          ))}
          {offersDiscidSubmission(result) && (
            <p className="muted small">
              この DiscID は MusicBrainz に未登録。候補を選んだら{' '}
              <a href={discidSubmissionUrl(result.discid, result.mb_toc)} target="_blank" rel="noopener noreferrer">
                MusicBrainz に DiscID を登録
              </a>
              しておくと次からは DiscID で当たる（ブラウザで登録）
            </p>
          )}
          {split.cd.length > 0 && (
            <ul className="cd-candidates">
              {split.cd.map((c) => (
                <li key={`${c.release_id}:${c.medium_position}`}>
                  <label>
                    <input
                      type="radio"
                      name="cd-candidate"
                      checked={cd.selected === indexOf(c)}
                      disabled={confirmed != null}
                      onChange={() => cd.select(indexOf(c))}
                    />{' '}
                    <strong>{c.artist}</strong> — {c.title}
                    <span className={c.exact ? 'badge' : 'badge muted'}>{matchedByLabel(c)}</span>{' '}
                    <a
                      href={releaseUrl(c)}
                      target="_blank"
                      rel="noopener noreferrer"
                      className="small"
                      title="MusicBrainz のリリースのページを開く"
                    >
                      MusicBrainz で見る
                    </a>
                    <div className="muted small">{candidateDetail(c, result.tracks)}</div>
                  </label>
                </li>
              ))}
            </ul>
          )}
          {split.other.length > 0 && (
            <div className="cd-other">
              <label className="small">
                <input type="checkbox" checked={showOther} onChange={(e) => setShowOther(e.target.checked)} />{' '}
                CD 以外の媒体に当たった候補も表示（{split.other.length} 件。デジタル配信・DVD・Blu-ray など、
                このドライブでは吸い出せない）
              </label>
              {showOther && (
                <ul className="cd-candidates">
                  {split.other.map((c) => (
                    <li key={`${c.release_id}:${c.medium_position}`}>
                      <label>
                        <input
                          type="radio"
                          name="cd-candidate"
                          checked={cd.selected === indexOf(c)}
                          disabled={confirmed != null}
                          onChange={() => cd.select(indexOf(c))}
                        />{' '}
                        <strong>{c.artist}</strong> — {c.title}
                        <span className="badge muted">{matchedByLabel(c)}</span>{' '}
                        <a href={releaseUrl(c)} target="_blank" rel="noopener noreferrer" className="small">
                          MusicBrainz で見る
                        </a>
                        <div className="muted small">{candidateDetail(c, result.tracks)}</div>
                      </label>
                    </li>
                  ))}
                </ul>
              )}
            </div>
          )}
          {result.candidates.length > 0 && (
            <fieldset className="cd-copy-scope">
              <legend className="small">候補から写す範囲</legend>
              {(Object.keys(COPY_SCOPE_LABELS) as CopyScope[]).map((scope) => (
                <label key={scope} className="small">
                  <input
                    type="radio"
                    name="cd-copy-scope"
                    checked={cd.copyScope === scope}
                    disabled={confirmed != null}
                    onChange={() => cd.setCopyScope(scope)}
                  />{' '}
                  {COPY_SCOPE_LABELS[scope]}
                </label>
              ))}
              <span className="muted small">
                最小限はアルバム名・アルバムアーティスト・日付・ディスク番号 / 枚数・MusicBrainz のリリース id。トラック名は貼り付けか手入力で埋める。
                切り替えると選択中の候補を写し直す（編集中の内容は消える）
              </span>
            </fieldset>
          )}
          <details className="cd-details">
            <summary className="small">詳細（ID・用語・手入力の TOC）</summary>
            <dl className="cd-ids small">
              <dt title="MusicBrainz がディスクを識別する ID。TOC から計算する">MusicBrainz DiscID</dt>
              <dd>
                <code>{result.discid}</code>
              </dd>
              <dt title="吸い出した音声の照合に使う DB の ID（AccurateRip）">AccurateRip</dt>
              <dd>
                <code>{result.accuraterip_id}</code>
              </dd>
              <dt title="CUETools Database。吸い出しの照合と傷の修復に使う">CTDB TOCID</dt>
              <dd>
                <code>{result.ctdb_toc_id}</code>
              </dd>
              <dt title="Table Of Contents。ディスクのトラックの開始位置">TOC</dt>
              <dd>
                <code>{cd.toc}</code>
              </dd>
              <dt>音声トラック</dt>
              <dd>{result.tracks.length} 曲</dd>
            </dl>
            <p className="muted small">
              用語: <b>DiscID</b> は TOC から計算する MusicBrainz のディスク識別子、<b>ISRC</b> は録音ごとの国際コード、
              <b>JAN/UPC</b> は商品のバーコード、<b>MBID</b> は MusicBrainz の各項目の ID、<b>medium</b> は
              リリースに入っている 1 枚（CD / DVD / Blu-ray / デジタル）。候補のバッジは当たった経路
              （DiscID 一致 / 指定 / ISRC / バーコード / TOC 近似）
            </p>
            <div className="op-row">
              <textarea
                aria-label="TOC"
                rows={2}
                value={cd.toc}
                disabled={cd.busy}
                placeholder="0:22593:41700:…（ドライブが無い環境用。CTDB 形式 / MusicBrainz 形式 / cdrecord -toc の出力）"
                onChange={(e) => cd.setToc(e.target.value)}
              />
            </div>
            <div className="op-row">
              <button type="button" disabled={cd.busy} onClick={() => void cd.lookupToc(cd.toc, { ...ids, refresh: true })}>
                この TOC で照会
              </button>
            </div>
            <div className="op-row">
              <input
                type="text"
                className="cd-release-ref"
                aria-label="MusicBrainz のリリース URL か MBID"
                placeholder="https://musicbrainz.org/release/… か MBID（候補に出ない盤はこれで当てる）"
                value={releaseRef}
                disabled={cd.busy}
                onChange={(e) => setReleaseRef(e.target.value)}
              />
              <button
                type="button"
                disabled={cd.busy || releaseRef.trim() === ''}
                onClick={() => void cd.lookupToc(cd.toc, { ...ids, release: releaseRef, refresh: true })}
              >
                このリリースで照会
              </button>
            </div>
          </details>
          {confirmed == null && (
            <div className="op-row">
              <button type="button" disabled={draft?.source === 'manual'} onClick={cd.startManual}>
                {result.candidates.length > 0 ? '候補を使わず手入力' : '手入力'}
              </button>
              {result.candidates.length > 0 && (
                <span className="muted small">候補を選ぶとフォームに写る（編集中の内容は写し直しで消える）</span>
              )}
            </div>
          )}
        </>
      )}

      {draft != null && confirmed == null && <DraftForm cd={cd} />}
      {confirmed != null && <Confirmed meta={confirmed} onEdit={cd.unconfirm} />}
    </section>
  )
}

function DraftForm({ cd }: { cd: CdLookupState }) {
  const d = cd.draft!
  const text = (label: string, key: 'album' | 'album_artist' | 'date' | 'label' | 'catalog_number' | 'barcode', hint?: string) => (
    <label className="cd-field">
      <span>{label}</span>
      <input type="text" value={d[key]} placeholder={hint} onChange={(e) => cd.updateDraft({ [key]: e.target.value })} />
    </label>
  )
  const num = (label: string, key: 'disc_no' | 'disc_count') => (
    <label className="cd-field cd-field-num">
      <span>{label}</span>
      <input
        type="number"
        min={1}
        max={99}
        value={d[key]}
        onChange={(e) => {
          const v = Number.parseInt(e.target.value, 10)
          if (Number.isFinite(v) && v >= 1) cd.updateDraft({ [key]: v })
        }}
      />
    </label>
  )
  return (
    <>
      <h2>
        メタデータ{' '}
        {d.source === 'musicbrainz' ? (
          <span className="badge">MusicBrainz の候補を土台に編集</span>
        ) : (
          <span className="badge muted">手入力</span>
        )}
      </h2>
      <div className="cd-form">
        {text('アルバム', 'album')}
        {text('アルバムアーティスト', 'album_artist')}
        {text('日付', 'date', 'YYYY-MM-DD')}
        {text('レーベル', 'label')}
        {text('カタログ番号', 'catalog_number')}
        {text('JAN/UPC', 'barcode')}
        {num('ディスク', 'disc_no')}
        {num('枚数', 'disc_count')}
        <CategoryField value={d.category} onChange={(v) => cd.updateDraft({ category: v })} />
      </div>

      <h2>トラックリスト貼り付け</h2>
      <p className="muted small">
        通販ページ等のテキストを 1 行 1 曲で貼る。行頭の番号（<code>1.</code> <code>01</code> <code>M-1</code>）と行末の時間は
        外し、<code>タイトル / アーティスト</code>（<code>／</code> <code>|</code> <code>-</code> も）で分ける。表（タブ区切り）も可。
        番号で行に写すので、番号が無ければ上から順
      </p>
      <div className="op-row">
        <textarea
          aria-label="トラックリスト"
          rows={6}
          value={cd.paste}
          placeholder={'1. タイトル / アーティスト 4:32\n2. …'}
          onChange={(e) => cd.setPaste(e.target.value)}
        />
      </div>
      <div className="op-row">
        <button type="button" disabled={cd.paste.trim() === ''} onClick={cd.applyPaste}>
          行に写す
        </button>
        <label className="small">
          <input
            type="checkbox"
            checked={cd.pasteArtistFirst}
            onChange={(e) => cd.setPasteArtistFirst(e.target.checked)}
          />{' '}
          アーティスト / タイトル の順で書かれている
        </label>
      </div>
      {cd.pasteWarnings.length > 0 && (
        <ul className="cd-warnings small">
          {cd.pasteWarnings.map((w) => (
            <li key={w}>{w}</li>
          ))}
        </ul>
      )}

      <h2>トラック（番号と長さは TOC から）</h2>
      <table className="cd-tracks cd-tracks-edit">
        <thead>
          <tr>
            <th>#</th>
            <th>タイトル</th>
            <th>アーティスト（空ならアルバムアーティスト）</th>
            <th>長さ</th>
            <th>ISRC</th>
          </tr>
        </thead>
        <tbody>
          {d.tracks.map((t, i) => (
            <tr key={t.number}>
              <td>{t.number}</td>
              <td>
                <input
                  type="text"
                  aria-label={`トラック ${t.number} のタイトル`}
                  value={t.title}
                  onChange={(e) => cd.updateTrack(i, { title: e.target.value })}
                />
              </td>
              <td>
                <input
                  type="text"
                  aria-label={`トラック ${t.number} のアーティスト`}
                  value={t.artist}
                  placeholder={d.album_artist}
                  onChange={(e) => cd.updateTrack(i, { artist: e.target.value })}
                />
              </td>
              <td>{formatDuration(t.length_ms)}</td>
              <td className="muted small">{t.mb?.isrcs.join(', ') ?? ''}</td>
            </tr>
          ))}
        </tbody>
      </table>
      <div className="op-row">
        <button type="button" className="primary" onClick={cd.confirm}>
          この内容で確定
        </button>
        <button type="button" onClick={cd.fillTitles}>
          空のタイトルを Track NN で埋める
        </button>
      </div>
      {cd.draftErrors.length > 0 && (
        <ul className="cd-errors small">
          {cd.draftErrors.map((e) => (
            <li key={e} className="error">
              {e}
            </li>
          ))}
        </ul>
      )}
    </>
  )
}

/** 配置先の category（統制語彙から選ぶ。無ければ _Unsorted。その場で語彙を足せる）。Inbox タブでも使う */
export function CategoryField({ value, onChange }: { value: string | null; onChange: (v: string | null) => void }) {
  const cats = useCategories(true)
  const [adding, setAdding] = useState('')
  const add = async () => {
    const name = adding.trim()
    if (name === '') return
    const c = await cats.create(name)
    if (c != null) {
      onChange(c.name)
      setAdding('')
    }
  }
  return (
    <label className="cd-field cd-field-category">
      <span>category（配置先。未選択なら _Unsorted）</span>
      <select value={value ?? ''} onChange={(e) => onChange(e.target.value === '' ? null : e.target.value)}>
        <option value="">（未分類 → _Unsorted）</option>
        {cats.items.map((c) => (
          <option key={c.id} value={c.name}>
            {c.name}
          </option>
        ))}
      </select>
      <span className="cd-category-add">
        <input
          type="text"
          aria-label="新しい category"
          placeholder="語彙に追加"
          value={adding}
          onChange={(e) => setAdding(e.target.value)}
        />
        <button type="button" disabled={adding.trim() === ''} onClick={add}>
          追加
        </button>
      </span>
      {cats.error != null && <span className="error">{cats.error}</span>}
    </label>
  )
}

function Confirmed({ meta, onEdit }: { meta: DiscMetadata; onEdit: () => void }) {
  return (
    <>
      <h2>
        確定: {meta.album_artist} — {meta.album}{' '}
        {meta.source === 'musicbrainz' ? <span className="badge">MusicBrainz</span> : <span className="badge muted">手入力</span>}
      </h2>
      <p className="muted small">配置先の category: {meta.category ?? '_Unsorted'}</p>
      <dl className="cd-ids small">
        {albumTags(meta).map(([k, values]) => (
          <Fragment key={k}>
            <dt>{k}</dt>
            <dd>{values.join(' / ')}</dd>
          </Fragment>
        ))}
      </dl>
      <table className="cd-tracks">
        <thead>
          <tr>
            <th>TRACKNUMBER</th>
            <th>TITLE</th>
            <th>ARTIST</th>
            <th>MUSICBRAINZ_TRACKID / RELEASETRACKID / ISRC</th>
          </tr>
        </thead>
        <tbody>
          {meta.tracks.map((t) => {
            // 表示だけ（書き込みの写像は trackTags そのもの。多値の ISRC は列内で , 区切り）
            const tags = trackTags(t).filter(([k]) => k.startsWith('MUSICBRAINZ_') || k === 'ISRC')
            return (
              <tr key={t.number}>
                <td>{t.number}</td>
                <td>{t.title}</td>
                <td>{t.artist}</td>
                <td className="muted small">{tags.map(([, values]) => values.join(', ')).join(' / ')}</td>
              </tr>
            )
          })}
        </tbody>
      </table>
      <div className="op-row">
        <button type="button" onClick={onEdit}>
          編集に戻る
        </button>
        <span className="muted small">
          吸い出しと配置は P2-5 / P2-8。この内容がそのままタグになる（TRACKTOTAL と MUSICBRAINZ_DISCID は TOC から）
        </span>
      </div>
    </>
  )
}
