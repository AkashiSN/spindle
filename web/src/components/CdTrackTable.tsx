// CD 画面のトラック表（P4-20）。行は TOC の音声トラックと 1:1 で、**照会の前から出る**
// （`GET /api/cd/status` の `tracks`）。
//
// **読み取り専用**（P4-20 追記）。CD 画面は「何が入っていて、どの盤として取り込むか」を見せる場所で、
// 値を直すのは Inbox の承認画面（吸い出したものは Inbox を通る。D-67 追記）。入力欄を置くと直す場所が
// 2 つになり、どちらが効くのか分からなくなる。
//
// 見た目はライブラリの表に寄せる（`--row-h` の行高、sticky なヘッダ、行のホバー、番号と長さは右寄せ）。
// 名前の分からないトラックは `Track NN` を薄字で出す（取り込むとこの名前になる）。
//
// 取り込み中だけ右端に進捗が出る（`progress` が null なら列ごと出さない）。

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

export function CdTrackTable({ draft, progress }: { draft: DiscDraft; progress: RipProgress | null }) {
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
        {draft.tracks.map((t) => {
          const title = t.title.trim()
          const artist = t.artist.trim()
          return (
            <tr key={t.number}>
              {/* ISRC は列にしない（12 桁が並ぶと表が読めない）。番号のツールチップに出す */}
              <td
                className="num"
                title={t.mb != null && t.mb.isrcs.length > 0 ? `ISRC: ${t.mb.isrcs.join(', ')}` : undefined}
              >
                {t.number}
              </td>
              {/* 名前が入っていない行は、取り込んだときに付く名前を薄字で見せる */}
              <td className={title === '' ? 'muted' : undefined}>{title === '' ? defaultTitle(t.number) : title}</td>
              <td className={artist === '' ? 'muted' : undefined}>
                {artist === '' ? draft.album_artist.trim() : artist}
              </td>
              <td className="num">{formatDuration(t.length_ms)}</td>
              {progress != null && <td className="muted small">{progressCell(t, progress)}</td>}
            </tr>
          )
        })}
      </tbody>
    </table>
  )
}
