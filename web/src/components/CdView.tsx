// CD 画面（SPEC §12.6 のウィザード。P2-3 の「候補選択」と P2-4 の「手入力 / トラックリスト貼り付け」まで）。
// ドライブの検出（P2-1）で TOC が出たら自動で照会し、貼り付け欄はドライブ無しの環境用に残す。
// 候補を選ぶとフォームに写り、そこから直せる。
// 候補ゼロ件でも空のフォームで完走できる（D-21）。吸い出し（P2-5）は後続

import { Fragment, useState } from 'react'
import { useCategories } from '../hooks/useCategories'
import type { CdDriveState } from '../hooks/useCdDrive'
import type { CdLookupState } from '../hooks/useCdLookup'
import { driveStateLabel } from '../lib/cdDrive'
import {
  albumTags,
  candidateLengthMs,
  candidateSummary,
  COPY_SCOPE_LABELS,
  lookupHeadline,
  trackTags,
  type CopyScope,
  type DiscMetadata,
} from '../lib/cd'
import { formatDuration } from '../lib/format'

export function CdView({ cd, drive }: { cd: CdLookupState; drive: CdDriveState }) {
  const { result, draft, confirmed } = cd
  const driveLabel = drive.unavailable ? null : driveStateLabel(drive.status)
  const canEject =
    drive.status != null && drive.status.state !== 'no_drive' && drive.status.state !== 'unknown' && !drive.ejecting
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
          {drive.unavailable ? 'CD ドライブは配線されていない' : (driveLabel ?? '…')}
        </span>
        {!drive.unavailable && (
          <button type="button" className="ghost" disabled={!canEject} onClick={() => void drive.eject()}>
            {drive.ejecting ? '取り出し中…' : '取り出す'}
          </button>
        )}
      </div>
      {drive.error != null && <p className="error">{drive.error}</p>}

      <h2>TOC</h2>
      <p className="muted small">
        ディスクを入れると TOC が入り、照会が自動で始まる。ドライブが無い環境では貼り付けて照会する:
        CTDB 形式 <code>0:13915:25592:…:リードアウト</code>（データトラックは <code>-</code> 前置）、
        MusicBrainz 形式 <code>1 12 リードアウト+150 各オフセット+150…</code>、または <code>cdrecord -toc</code> の出力
      </p>
      <div className="op-row">
        <textarea
          aria-label="TOC"
          rows={3}
          value={cd.toc}
          disabled={cd.busy}
          placeholder="0:22593:41700:58133:71920:91198:104468:115188:131988:143758:159678:174415:191880"
          onChange={(e) => cd.setToc(e.target.value)}
        />
      </div>
      <div className="op-row">
        <button type="button" className="primary" disabled={cd.busy} onClick={() => void cd.lookup()}>
          {cd.busy ? '照会中…' : 'MusicBrainz に照会'}
        </button>
        <span className="muted small">UA 付き・1 秒 1 回。同人・VTuber・インディーズの国内盤は未登録が普通</span>
      </div>
      {cd.error != null && <p className="error">{cd.error}</p>}

      {result != null && (
        <>
          <h2>{lookupHeadline(result)}</h2>
          <dl className="cd-ids small">
            <dt>MusicBrainz DiscID</dt>
            <dd>
              <code>{result.discid}</code>
            </dd>
            <dt>AccurateRip</dt>
            <dd>
              <code>{result.accuraterip_id}</code>
            </dd>
            <dt>CTDB TOCID</dt>
            <dd>
              <code>{result.ctdb_toc_id}</code>
            </dd>
            <dt>音声トラック</dt>
            <dd>{result.tracks.length} 曲</dd>
          </dl>
          {result.candidates.length > 0 && (
            <ul className="cd-candidates">
              {result.candidates.map((c, i) => (
                <li key={`${c.release_id}:${c.medium_position}`}>
                  <label>
                    <input
                      type="radio"
                      name="cd-candidate"
                      checked={cd.selected === i}
                      disabled={confirmed != null}
                      onChange={() => cd.select(i)}
                    />{' '}
                    <strong>{c.artist}</strong> — {c.title}
                    {c.exact ? <span className="badge">DiscID 一致</span> : <span className="badge muted">近似</span>}
                    <div className="muted small">
                      {candidateSummary(c)} · {c.tracks.length} 曲
                      {candidateLengthMs(c) != null ? ` · ${formatDuration(candidateLengthMs(c))}` : ''}
                    </div>
                  </label>
                </li>
              ))}
            </ul>
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
