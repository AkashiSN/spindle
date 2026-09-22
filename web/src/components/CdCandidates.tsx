// CD 画面の候補（P4-20）。左に選択中の候補のジャケット（D-82。無ければ同じ寸法の空枠でレイアウトを
// 動かさない）、右に候補のラジオ一覧。
//
// 候補は MusicBrainz へのリンク・収録構成（DVD 付き / BD 付き / デジタルの別）・ディスクとの長さ差で
// 見分ける。CD 以外の medium に当たった候補は既定で畳む。まだ引いていない段があれば（`can_widen`）
// 「さらに広げて探す」を出す（D-64 追記 4）。

import { useState } from 'react'
import type { CdLookupState } from '../hooks/useCdLookup'
import {
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
  type CopyScope,
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

/** 選択中の候補のジャケット。無い盤・取れない盤は同じ寸法の空枠 */
function Cover({ releaseId }: { releaseId: string | null }) {
  const [failed, setFailed] = useState<string | null>(null)
  if (releaseId == null || failed === releaseId) {
    return <div className="cd-cover empty" aria-hidden="true" />
  }
  return (
    <img
      className="cd-cover"
      src={`/api/cd/cover/${releaseId}`}
      alt="候補のジャケット"
      onError={() => setFailed(releaseId)}
    />
  )
}

function CandidateItem({
  c,
  index,
  checked,
  tocTracks,
  onSelect,
}: {
  c: ReleaseCandidate
  index: number
  checked: boolean
  tocTracks: { number: number; length_ms: number }[]
  onSelect: (i: number) => void
}) {
  return (
    <li>
      <label>
        <input type="radio" name="cd-candidate" checked={checked} onChange={() => onSelect(index)} />{' '}
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
        <div className="muted small">{candidateDetail(c, tocTracks)}</div>
      </label>
    </li>
  )
}

export function CdCandidates({ cd }: { cd: CdLookupState }) {
  const { result } = cd
  const [showOther, setShowOther] = useState(false)
  // 別の結果になったら「CD 以外」は畳み直す（既定で隠すため。描画中に直す React の作法で、
  // effect にすると 1 回余計に描画される）
  const [shownFor, setShownFor] = useState(result)
  if (shownFor !== result) {
    setShownFor(result)
    setShowOther(false)
  }
  if (result == null) return null

  const split = splitByMedium(result.candidates)
  // 候補の番号は元の一覧のもの（選択は index で持つ）
  const indexOf = (c: ReleaseCandidate) => result.candidates.indexOf(c)
  const selected = cd.selected != null ? (result.candidates[cd.selected] ?? null) : null
  const otherOpen = showOther || (selected != null && split.other.includes(selected))

  return (
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
      <div className="cd-candidates-row">
        <Cover releaseId={selected?.release_id ?? null} />
        <div>
          {split.cd.length > 0 && (
            <ul className="cd-candidates">
              {split.cd.map((c) => (
                <CandidateItem
                  key={`${c.release_id}:${c.medium_position}`}
                  c={c}
                  index={indexOf(c)}
                  checked={cd.selected === indexOf(c)}
                  tocTracks={result.tracks}
                  onSelect={cd.select}
                />
              ))}
            </ul>
          )}
          {split.other.length > 0 && (
            <div className="cd-other">
              <label className="small">
                <input type="checkbox" checked={otherOpen} onChange={(e) => setShowOther(e.target.checked)} />{' '}
                CD 以外の媒体に当たった候補も表示（{split.other.length} 件。デジタル配信・DVD・Blu-ray など、
                このドライブでは吸い出せない）
              </label>
              {otherOpen && (
                <ul className="cd-candidates">
                  {split.other.map((c) => (
                    <CandidateItem
                      key={`${c.release_id}:${c.medium_position}`}
                      c={c}
                      index={indexOf(c)}
                      checked={cd.selected === indexOf(c)}
                      tocTracks={result.tracks}
                      onSelect={cd.select}
                    />
                  ))}
                </ul>
              )}
            </div>
          )}
          {result.can_widen && (
            <div className="op-row">
              <button type="button" disabled={cd.busy} onClick={() => void cd.widen()}>
                さらに広げて探す
              </button>
              <span className="muted small">
                TOC の近さでも探す（トラック長の違う別の盤が混ざるので、どれも違うときだけ）
              </span>
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
                    onChange={() => cd.setCopyScope(scope)}
                  />{' '}
                  {COPY_SCOPE_LABELS[scope]}
                </label>
              ))}
              <span className="muted small">
                既定の「全部写す」はトラック名・アーティスト・レーベル・カタログ番号・JAN/UPC まで写す。
                「最小限」にすると盤を見分けるのに要るものだけ（アルバム名・アルバムアーティスト・
                日付・ディスク番号 / 枚数・MusicBrainz のリリース id）になり、トラック名は貼り付けか
                手入力で埋める。切り替えると選択中の候補を写し直す（編集中の内容は消える）
              </span>
            </fieldset>
          )}
          {result.candidates.length > 0 && (
            <div className="op-row">
              <button type="button" disabled={cd.draft?.source === 'manual'} onClick={cd.startManual}>
                候補を使わず手入力
              </button>
              <span className="muted small">候補を選ぶと表に写る（編集中の内容は写し直しで消える）</span>
            </div>
          )}
        </div>
      </div>
    </>
  )
}
