// CD 画面（SPEC §12.6、P2-3 / P2-4 / P4-20）。ドライブに CD が入っている前提で、
// **上がディスクのトラック表、下が MusicBrainz の候補**。左カラム（ツリー）は出さない（§12.1）。
//
// 表は照会の前から出る（`GET /api/cd/status` の `tracks`）。タイトルは `Track NN` のプレースホルダで、
// 候補を選ぶと実名が入る。「確定」の段は無く、表を直接編集して「取り込む」（吸い出しは P2-5 なので
// いまは無効）。TOC の貼り付けと各種 ID は「詳細」の中（ドライブ無しの環境とデバッグ用）。

import { useState } from 'react'
import type { CdDriveState } from '../hooks/useCdDrive'
import type { CdLookupState } from '../hooks/useCdLookup'
import { driveIdsFor, driveStateLabel } from '../lib/cdDrive'
import { validateDraft } from '../lib/cd'
import { CdAlbumFields } from './CdAlbumFields'
import { CdCandidates } from './CdCandidates'
import { CdTrackTable } from './CdTrackTable'

export function CdView({ cd, drive }: { cd: CdLookupState; drive: CdDriveState }) {
  const { draft } = cd
  const driveLabel = drive.unavailable ? null : driveStateLabel(drive.status)
  // ドライブから読めた ISRC / MCN は、欄の TOC がその盤のものであるときだけ照会に添える
  // （別の盤の TOC を貼ったときに混ぜない。lib/cdDrive.ts の driveIdsFor）
  const ids = driveIdsFor(cd.toc, drive.status)
  const [releaseRef, setReleaseRef] = useState('')
  const canEject =
    drive.status != null && drive.status.state !== 'no_drive' && drive.status.state !== 'unknown' && !drive.ejecting
  const hasDisc = drive.status?.state === 'disc_ok'
  const errors = draft != null ? validateDraft(draft) : []

  return (
    <section className="cd">
      <div className="table-toolbar">
        <h1>CD</h1>
        <span className="spacer" />
        {(cd.result != null || draft != null) && (
          <button type="button" className="ghost" onClick={cd.reset}>
            結果を消す
          </button>
        )}
      </div>

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
          DiscID → 無ければディスクの{' '}
          <abbr title="International Standard Recording Code。録音ごとの国際コード。ディスクのサブチャネルに入っている">
            ISRC
          </abbr>{' '}
          /{' '}
          <abbr title="Japanese Article Number / Universal Product Code。商品のバーコード。ディスクに入っていれば読む">
            JAN/UPC
          </abbr>
          → それでも出なければ TOC 近似、の順に広げる。結果は 10 分覚えていて、「照会し直す」で引き直す
          {ids.isrcs.some((i) => i != null) ? `。ISRC: ${ids.isrcs.filter((i) => i != null).join(', ')}` : ''}
          {ids.mcn != null ? `。JAN/UPC: ${ids.mcn}` : ''}
        </p>
      )}
      {drive.error != null && <p className="error">{drive.error}</p>}
      {cd.error != null && <p className="error">{cd.error}</p>}

      {draft != null && (
        <>
          <CdAlbumFields cd={cd} />
          <CdTrackTable draft={draft} onTrack={cd.updateTrack} progress={null} readOnly={false} />
          <div className="op-row">
            <button type="button" className="primary" disabled title="吸い出しは P2-5 で実装する">
              取り込む
            </button>
            <span className="muted small">吸い出しと配置は P2-5 / P2-8。いまは内容の確認まで</span>
          </div>
          {errors.length > 0 && (
            <ul className="cd-errors small">
              {errors.map((e) => (
                <li key={e} className="error">
                  {e}
                </li>
              ))}
            </ul>
          )}
        </>
      )}

      <CdCandidates cd={cd} />

      <details className="cd-details">
        <summary className="small">詳細（ID・用語・手入力の TOC）</summary>
        {cd.result != null && (
          <dl className="cd-ids small">
            <dt title="MusicBrainz がディスクを識別する ID。TOC から計算する">MusicBrainz DiscID</dt>
            <dd>
              <code>{cd.result.discid}</code>
            </dd>
            <dt title="吸い出した音声の照合に使う DB の ID（AccurateRip）">AccurateRip</dt>
            <dd>
              <code>{cd.result.accuraterip_id}</code>
            </dd>
            <dt title="CUETools Database。吸い出しの照合と傷の修復に使う">CTDB TOCID</dt>
            <dd>
              <code>{cd.result.ctdb_toc_id}</code>
            </dd>
            <dt title="Table Of Contents。ディスクのトラックの開始位置">TOC</dt>
            <dd>
              <code>{cd.toc}</code>
            </dd>
            <dt>音声トラック</dt>
            <dd>{cd.result.tracks.length} 曲</dd>
          </dl>
        )}
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
    </section>
  )
}
