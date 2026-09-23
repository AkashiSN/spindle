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
// 取り込み中だけ右端に進捗が出る（`progress` が null なら列ごと出さない）。欄の言葉は lib/cdRip.ts の
// ripCellLabel（相ごとの「完了」の表現は P2-5 で確定）。

import { defaultTitle, type DiscDraft } from '../lib/cd'
import { ripCellLabel, type RipProgress } from '../lib/cdRip'
import { formatDuration } from '../lib/format'

export function CdTrackTable({ draft, progress }: { draft: DiscDraft; progress: RipProgress | null }) {
  const numbers = draft.tracks.map((t) => t.number)
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
              {progress != null && <td className="muted small">{ripCellLabel(t.number, progress, numbers)}</td>}
            </tr>
          )
        })}
      </tbody>
    </table>
  )
}
