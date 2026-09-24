// CD 画面の「① 挿入中の CD」（P4-20 追記）。**読み取り専用**。
//
// 以前はここが入力欄（アルバム名 / アーティスト / 日付 / category / レーベル…）だったが、
// 吸い出したものは Inbox を通るようにしたので（D-67 追記）、値を直すのは Inbox の承認画面に一本化した。
// ここは「どの盤として取り込むか」を確かめるための表示に徹する。
//
// 値はすべてラベル付きの定義リストで出す（アルバム名を裸の見出しにすると、何の値か分からない）。
// 候補を選んでいなければアルバム名は決まらない。その場合は「（候補を選んでいない）」と出す。

import type { DiscDraft } from '../lib/cd'

type Row = { label: string; value: string; empty?: string }

export function CdAlbumSummary({ draft }: { draft: DiscDraft }) {
  const rows: Row[] = [
    { label: 'アルバム', value: draft.album, empty: '（候補を選んでいない）' },
    { label: 'アルバムアーティスト', value: draft.album_artist, empty: '—' },
    { label: '日付', value: draft.date, empty: '—' },
    ...(draft.disc_count > 1 ? [{ label: 'ディスク', value: `${draft.disc_no} / ${draft.disc_count}` }] : []),
    { label: 'レーベル', value: draft.label },
    { label: 'カタログ番号', value: draft.catalog_number },
    { label: 'JAN/UPC', value: draft.barcode },
  ]
  return (
    <dl className="cd-summary">
      {rows
        // 補助の項目（レーベル以降）は値のあるときだけ
        .filter((r) => r.empty != null || r.value.trim() !== '')
        .map((r) => {
          const v = r.value.trim()
          return (
            <div key={r.label} className={r.label === 'アルバム' ? 'cd-summary-album' : undefined}>
              <dt>{r.label}</dt>
              <dd className={v === '' ? 'muted' : undefined}>{v === '' ? r.empty : v}</dd>
            </div>
          )
        })}
    </dl>
  )
}
