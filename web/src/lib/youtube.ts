// YouTube 画面の純粋ロジック（SPEC §12.6、D-70 追記、P4-13）: URL 欄の解析、`/youtube?url=` の受け口、
// ytdl ジョブの絞り込みと結果の 1 行表示

import type { Job } from '../api/types'

/** 1 行 1 URL。空行と前後の空白を落とし、重複は 1 つにする */
export function parseUrlLines(text: string): string[] {
  const out: string[] = []
  for (const line of text.split(/\r?\n/)) {
    const u = line.trim()
    if (u !== '' && !out.includes(u)) out.push(u)
  }
  return out
}

/** `/youtube?url=<URL>` で開かれたときの URL（ブックマークレットの受け口）。それ以外は null。
 *  http / https 以外（`javascript:` 等）は受けない */
export function urlFromLocation(pathname: string, search: string): string | null {
  if (pathname.replace(/\/+$/, '') !== '/youtube') return null
  const url = new URLSearchParams(search).get('url')?.trim() ?? ''
  if (!/^https?:\/\/\S+$/i.test(url)) return null
  return url
}

/** ytdl ジョブだけを新しい順に（同時刻は id の大きい方が先） */
export function ytdlJobs(items: readonly Job[]): Job[] {
  return items.filter((j) => j.type === 'ytdl').sort((a, b) => b.created_at - a.created_at || b.id - a.id)
}

/** 再生リストの URL か（`list=` を持つ。動画 URL に `list=` が付いた形も yt-dlp は playlist として扱う） */
export function isPlaylistUrl(url: string | null): boolean {
  if (url == null) return false
  try {
    return new URL(url).searchParams.has('list')
  } catch {
    return false
  }
}

/** 状態と結果を 1 行に。完了はサーバの `note`（Inbox に置いた / プラグインが skip / 再生リストを展開した）を
 *  そのまま出し、無ければ（0020 より前の行）「詳細なし」で断定しない。失敗の理由（取り込み済み・URL 不正 等。D-70）は
 *  `last_error` をそのまま見せる */
export function ytdlResultLabel(j: Job): string {
  const playlist = isPlaylistUrl(j.subject)
  switch (j.state) {
    case 'queued':
      return j.attempts > 0 && j.last_error ? `再試行待ち（${j.last_error}）` : '待ち'
    case 'running':
      return playlist ? '再生リストを展開中' : 'ダウンロード中'
    case 'done':
      // note の無い完了（0020 より前の行）は Inbox に置いたか skip か分からないので断定しない
      return j.note ?? '完了（詳細なし）'
    case 'failed':
      return j.last_error ?? '失敗'
    case 'cancelled':
      return '取り消し'
  }
}
