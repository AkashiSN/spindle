// CD 画面のアルバム要約（P4-20 追記）。**読み取り専用**。
//
// 以前はここが入力欄（アルバム名 / アーティスト / 日付 / category / レーベル…）だったが、
// 吸い出したものは Inbox を通るようにしたので（D-67 追記）、値を直すのは Inbox の承認画面に一本化した。
// ここは「どの盤として取り込むか」を確かめるための表示に徹する。
//
// 候補を選んでいなければアルバム名は決まらない。その場合は取り込んだときに付く名前
// （`Track NN` と、アルバム名の無いディレクトリ）になる旨を出す。

import type { DiscDraft } from '../lib/cd'

/** 表示する値が無ければ null を返す（空の項目は出さない） */
function line(label: string, value: string): [string, string] | null {
  const v = value.trim()
  return v === '' ? null : [label, v]
}

export function CdAlbumSummary({ draft }: { draft: DiscDraft }) {
  const album = draft.album.trim()
  const rows = [
    line('アルバムアーティスト', draft.album_artist),
    line('日付', draft.date),
    draft.disc_count > 1 ? (['ディスク', `${draft.disc_no} / ${draft.disc_count}`] as [string, string]) : null,
    line('レーベル', draft.label),
    line('カタログ番号', draft.catalog_number),
    line('JAN/UPC', draft.barcode),
  ].filter((r): r is [string, string] => r != null)

  return (
    <div className="cd-summary">
      <h2 className="cd-summary-title">
        {album === '' ? <span className="muted">（候補を選んでいない）</span> : album}
      </h2>
      {rows.length > 0 && (
        <dl className="cd-summary-meta small">
          {rows.map(([k, v]) => (
            <div key={k}>
              <dt>{k}</dt>
              <dd>{v}</dd>
            </div>
          ))}
        </dl>
      )}
      <p className="muted small">
        {album === ''
          ? '候補を選ばずに取り込むと、名前の付いていない盤として Inbox に入る。名前は Inbox の承認画面で入れる'
          : 'ここは表示だけ。取り込んだものは Inbox に入るので、名前や配置先（category）は承認画面で直す'}
      </p>
    </div>
  )
}
