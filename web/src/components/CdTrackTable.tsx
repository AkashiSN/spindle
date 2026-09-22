// CD 画面のトラック表（P4-20）。行は TOC の音声トラックと 1:1 で、**照会の前から出る**
// （`GET /api/cd/status` の `tracks`）。タイトルが空の行は `Track NN` をプレースホルダで見せ、
// 候補を選ぶと実名が入る（空のままなら確定時に `Track NN` が入る）。
//
// 取り込み中だけ右端に進捗が出る（`progress` が null なら列ごと出さないので、P2-5 が入るまで
// 表の見た目は変わらない）。

import { defaultTitle, type DiscDraft, type DiscTrackDraft } from '../lib/cd'
import { formatDuration } from '../lib/format'

/**
 * 吸い出しの進捗（P2-5 が SSE の `job` イベントで流す）。いまは器だけで、値は来ない。
 * 形は SPEC §7.2 と 1 対 1 にしておくこと（`disc_no` を落とさない）
 */
export type RipProgress = {
  phase: 'read' | 'verify' | 'encode' | 'place'
  /** 複数枚組のディスク番号（rip.log / disc<N>.cue と揃える。D-67） */
  disc_no: number
  /** いま処理しているトラック */
  track_no: number
  /** その相の完了数と総数 */
  done: number
  total: number
}

const PHASE_LABELS: Record<RipProgress['phase'], string> = {
  read: '読み取り',
  verify: '照合',
  encode: 'エンコード',
  place: '配置',
}

/**
 * 進捗の欄。**いまは `track_no` の行にだけ相の名前を出し、ほかの行は空にする。**
 * 吸い出しは「全ディスクを 1 本の PCM で取得 → 分割」（P2-5）なので `read` はトラック単位に
 * 分かれず、相によって `track_no` の進み方が違う。「このトラックは完了」の表現は、P2-5 が
 * 相の順序を確定してから足す（先に推測で書くと誤表示になる）
 */
function progressCell(t: DiscTrackDraft, progress: RipProgress | null): string {
  if (progress == null || t.number !== progress.track_no) return ''
  return PHASE_LABELS[progress.phase]
}

export function CdTrackTable({
  draft,
  onTrack,
  progress,
  readOnly,
}: {
  draft: DiscDraft
  onTrack: (index: number, patch: Partial<DiscTrackDraft>) => void
  progress: RipProgress | null
  readOnly: boolean
}) {
  return (
    <table className="cd-tracks">
      <thead>
        <tr>
          <th className="num">#</th>
          <th>タイトル</th>
          <th>アーティスト</th>
          <th className="num">長さ</th>
          {progress != null && <th>進捗</th>}
        </tr>
      </thead>
      <tbody>
        {draft.tracks.map((t, i) => (
          <tr key={t.number}>
            {/* ISRC は列にしない（12 桁が並ぶと表が読めない）。番号のツールチップに出す */}
            <td
              className="num"
              title={t.mb != null && t.mb.isrcs.length > 0 ? `ISRC: ${t.mb.isrcs.join(', ')}` : undefined}
            >
              {t.number}
            </td>
            <td>
              <input
                type="text"
                aria-label={`トラック ${t.number} のタイトル`}
                value={t.title}
                placeholder={defaultTitle(t.number)}
                disabled={readOnly}
                onChange={(e) => onTrack(i, { title: e.target.value })}
              />
            </td>
            <td>
              <input
                type="text"
                aria-label={`トラック ${t.number} のアーティスト`}
                value={t.artist}
                placeholder={draft.album_artist || 'アルバムアーティスト'}
                disabled={readOnly}
                onChange={(e) => onTrack(i, { artist: e.target.value })}
              />
            </td>
            <td className="num">{formatDuration(t.length_ms)}</td>
            {progress != null && <td className="muted small">{progressCell(t, progress)}</td>}
          </tr>
        ))}
      </tbody>
    </table>
  )
}
