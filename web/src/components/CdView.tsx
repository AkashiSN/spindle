// CD 画面（SPEC §12.6、P2-3 / P2-4 / P4-20）。ドライブに CD が入っている前提で、上から
// **① 挿入中の CD → ② 認識したトラック → ③ 候補を選ぶ → ④ 取り込む** の番号付きの段で流れを見せる。
// 最上部にドライブの状態と、その盤がライブラリにあるかの帯（`POST /api/cd/library`）。
// 左カラム（ツリー）は出さない（§12.1）。長い説明は各段の見出しの ⓘ（Hint）に畳む。
//
// 表は照会の前から出る（`GET /api/cd/status` の `tracks`）。名前の分からないトラックは `Track NN` を
// 薄字で見せ、候補を選ぶと実名が入る。
//
// **この画面では編集させない**（P4-20 追記）。吸い出したものは Inbox を通るので（D-67 追記）、
// 値を直すのは Inbox の承認画面に一本化してある。ここは「何が入っていて、どの盤として取り込むか」を
// 確かめる場所。TOC の貼り付けとリリース URL の指定は編集ではなく照会の入力なので「詳細」に残す
// （ドライブ無しの環境とデバッグ用）。

import { useEffect, useState } from 'react'
import type { CdDriveState } from '../hooks/useCdDrive'
import { useCdLibrary } from '../hooks/useCdLibrary'
import type { CdLookupState } from '../hooks/useCdLookup'
import type { CdRipState } from '../hooks/useCdRip'
import { lookupHeadline, validateDraft } from '../lib/cd'
import { driveIdsFor, driveInfoLabel, driveStateLabel } from '../lib/cdDrive'
import { libraryNotice } from '../lib/cdLibrary'
import { ripStatusLabel } from '../lib/cdRip'
import { CdAlbumSummary } from './CdAlbumSummary'
import { CdCandidates } from './CdCandidates'
import { CdTrackTable } from './CdTrackTable'
import { Hint } from './Hint'
import { Step } from './Step'

export function CdView({
  cd,
  drive,
  rip,
  onOpenInbox,
  onOpenAlbum,
}: {
  cd: CdLookupState
  drive: CdDriveState
  rip: CdRipState
  onOpenInbox: () => void
  onOpenAlbum: (albumId: number) => void
}) {
  const { draft } = cd
  // 画面を開き直したとき、進行中の吸い出しを追い直す
  const ripJob = drive.status?.rip_job
  const adopt = rip.adopt
  useEffect(() => adopt(ripJob), [adopt, ripJob])
  const driveLabel = drive.unavailable ? null : driveStateLabel(drive.status)
  // ドライブから読めた ISRC / MCN は、欄の TOC がその盤のものであるときだけ照会に添える
  // （別の盤の TOC を貼ったときに混ぜない。lib/cdDrive.ts の driveIdsFor）
  const ids = driveIdsFor(cd.toc, drive.status)
  const [releaseRef, setReleaseRef] = useState('')
  const canEject =
    drive.status != null && drive.status.state !== 'no_drive' && drive.status.state !== 'unknown' && !drive.ejecting
  const hasDisc = drive.status?.state === 'disc_ok'
  // 取り込めるのは、表の TOC がいまドライブに入っている盤のものであるとき（貼り付けた TOC は吸えない）
  const discToc = hasDisc ? (drive.status?.toc ?? null) : null
  const ripProblems = draft != null ? validateDraft(draft) : []
  const canRip =
    draft != null && discToc != null && discToc === cd.toc && ripProblems.length === 0 && !rip.running && !rip.starting
  const owned = libraryNotice(useCdLibrary(draft != null ? cd.toc : '', draft?.release_id ?? null))
  const result = cd.result
  return (
    <section className="cd">
      <div className="table-toolbar">
        <h1>CD</h1>
        <span className="cd-drive-state small" aria-live="polite">
          {drive.unavailable ? 'CD ドライブが使えない（コンテナにデバイスが渡っていない）' : (driveLabel ?? '…')}
        </span>
        {!drive.unavailable && drive.status?.drive != null && (
          <Hint>ドライブ: {driveInfoLabel(drive.status.drive)}</Hint>
        )}
        <span className="spacer" />
        {!drive.unavailable && (
          <>
            <button
              type="button"
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
        {(result != null || draft != null) && (
          <button type="button" className="ghost" onClick={cd.reset}>
            結果を消す
          </button>
        )}
      </div>

      {owned != null && (
        <div className={`cd-owned cd-owned-${owned.kind}`} role="status">
          <span>
            {owned.kind === 'disc' ? '✓ ' : ''}
            {owned.text}
          </span>
          <button type="button" className="ghost" onClick={() => onOpenAlbum(owned.albumId)}>
            ライブラリで開く
          </button>
        </div>
      )}
      {drive.error != null && <p className="error">{drive.error}</p>}
      {cd.error != null && <p className="error">{cd.error}</p>}

      {draft == null ? (
        !drive.unavailable &&
        !hasDisc && <p className="muted cd-empty">CD を入れると自動で読み取り、MusicBrainz に照会する</p>
      ) : (
        <>
          <Step
            no={1}
            title="挿入中の CD"
            aside={<span className="badge muted">表示のみ</span>}
            hint={
              <>
                ③ で選んだ候補の値。ここでは直せない（表示のみ）。取り込んだものは Inbox に入るので、名前や
                配置先（category）は Inbox の承認画面で直す。候補を選ばずに取り込むと、名前の付いていない盤として
                Inbox に入る
              </>
            }
          >
            <CdAlbumSummary draft={draft} />
          </Step>

          <Step
            no={2}
            title="認識したトラック"
            aside={<span className="muted">{draft.tracks.length} 曲</span>}
            hint={
              <>
                ディスクの TOC から読んだ音声トラック。番号と長さはディスクから、名前は ③ の候補から入る。
                薄字は取り込んだときに付く名前（Track NN / アルバムアーティスト）。# にカーソルを置くと ISRC
              </>
            }
          >
            <CdTrackTable draft={draft} progress={rip.running ? rip.progress : null} />
          </Step>

          <Step
            no={3}
            title="候補を選ぶ"
            aside={
              <span className="muted">
                {result != null ? lookupHeadline(result) : cd.busy ? 'MusicBrainz に照会中…' : '未照会'}
              </span>
            }
            hint={
              <>
                DiscID → 無ければディスクの ISRC（録音ごとの国際コード）/ JAN/UPC（商品のバーコード）→
                それでも出なければ TOC 近似、の順に広げて MusicBrainz を引く。結果は 10 分覚えていて、
                「照会し直す」で引き直す。候補のバッジは当たった経路
                {ids.isrcs.some((i) => i != null) ? `。ISRC: ${ids.isrcs.filter((i) => i != null).join(', ')}` : ''}
                {ids.mcn != null ? `。JAN/UPC: ${ids.mcn}` : ''}
              </>
            }
          >
            <CdCandidates cd={cd} />
          </Step>

          <Step
            no={4}
            title="取り込む"
            hint={<>吸い出して AccurateRip / CTDB で照合し、Inbox に置く。名前や配置先は Inbox の承認画面で直す</>}
          >
            <div className="op-row">
              <button
                type="button"
                className="primary"
                disabled={!canRip}
                onClick={() => discToc != null && void rip.start(discToc, draft)}
              >
                {rip.starting ? '開始中…' : rip.running ? '取り込み中…' : '取り込む'}
              </button>
              <span className="muted small" aria-live="polite">
                {rip.running
                  ? rip.progress != null
                    ? ripStatusLabel(rip.progress)
                    : '待っている（ほかのジョブの後に始まる）'
                  : discToc == null
                    ? 'ドライブに盤が入っていると取り込める'
                    : discToc !== cd.toc
                      ? '表の TOC がドライブの盤と違う（照会し直す）'
                      : ripProblems.length > 0
                        ? ripProblems.join('、')
                        : draft.album.trim() === ''
                          ? '候補を選ばずに取り込む（名前は Inbox で入れる）'
                          : `『${draft.album.trim()}』として取り込む`}
              </span>
            </div>
            {rip.result != null && (
              <p className="small">
                {rip.result}{' '}
                <button type="button" className="ghost" onClick={onOpenInbox}>
                  Inbox を開く
                </button>
              </p>
            )}
            {rip.error != null && <p className="error">{rip.error}</p>}
          </Step>
        </>
      )}

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
