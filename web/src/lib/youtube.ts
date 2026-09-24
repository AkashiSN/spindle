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
 *  そのまま出し、無ければ（0020 より前の行）「詳細なし」で断定しない。失敗の理由（URL 不正 等。D-70。取り込み済みは失敗でなく完了の note。P4-18）は
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

// ---------------------------------------------------------------- ① の照合（D-87）

export type UrlKind = 'video' | 'playlist' | 'other' | 'invalid'

export type SubscriptionRef = { id: number; albumartist: string; album: string }

/** `POST /api/ytmusic/lookup` の 1 件 */
export type LookupItem = {
  url: string
  kind: UrlKind
  video_url?: string
  located?: { location: 'library' | 'inbox'; path: string }
  list_id?: string
  subscription?: SubscriptionRef
}

/** `POST /api/ytmusic/playlist` */
export type PlaylistInfo = {
  list_id: string
  title: string | null
  entries: number
  unavailable: number
  in_library: number
  in_inbox: number
  new: number
  truncated: boolean
  subscription?: SubscriptionRef
}

/** 再生リストの列挙の状態（URL ごと） */
export type PlaylistProbe = { state: 'loading' } | { state: 'ok'; info: PlaylistInfo } | { state: 'error'; message: string }

export type UrlTone = 'new' | 'skip' | 'bad' | 'pending'

/** ① の表の 1 行 */
export type UrlRow = {
  url: string
  kindLabel: string
  tone: UrlTone
  /** 状態の短い札（「新規」「ライブラリにある」…） */
  badge: string
  /** 札の後ろの説明 */
  detail: string
  /** ダウンロードに含めない */
  skip: boolean
}

const KIND_LABEL: Record<UrlKind, string> = { video: '動画', playlist: '再生リスト', other: 'その他', invalid: '—' }

function playlistDetail(info: PlaylistInfo): string {
  const rest: string[] = []
  if (info.in_library > 0) rest.push(`ライブラリに ${info.in_library}`)
  if (info.in_inbox > 0) rest.push(`Inbox に ${info.in_inbox}`)
  if (info.unavailable > 0) rest.push(`取れない ${info.unavailable}`)
  const title = info.title ?? '（題名なし）'
  const tail = rest.length > 0 ? `（${rest.join('・')} は飛ばす）` : ''
  return `${title} · ${info.entries} 本のうち新規 ${info.new} 本${tail}${info.truncated ? '。一部を取りこぼしている' : ''}`
}

/** ① の表の行。`lookup` がまだ無い（照合中）行は投入に含める（照合は目安で、投入側でも取り込み済みを飛ばす） */
export function urlRows(
  urls: readonly string[],
  lookup: ReadonlyMap<string, LookupItem>,
  probes: ReadonlyMap<string, PlaylistProbe>,
): UrlRow[] {
  return urls.map((url) => {
    const it = lookup.get(url)
    if (it == null) {
      return { url, kindLabel: isPlaylistUrl(url) ? '再生リスト' : '…', tone: 'pending', badge: '照合中', detail: '', skip: false }
    }
    const kindLabel = KIND_LABEL[it.kind]
    switch (it.kind) {
      case 'invalid':
        return { url, kindLabel, tone: 'bad', badge: '取れない', detail: 'http / https の URL ではない', skip: true }
      case 'other':
        return { url, kindLabel, tone: 'new', badge: 'YouTube 以外', detail: 'yt-dlp に任せる（取り込み済みかは照合しない）', skip: false }
      case 'video':
        if (it.located != null) {
          const where = it.located.location === 'library' ? 'ライブラリにある' : 'Inbox で承認待ち'
          return { url, kindLabel, tone: 'skip', badge: where, detail: `飛ばす（${it.located.path}）`, skip: true }
        }
        return { url, kindLabel, tone: 'new', badge: '新規', detail: '', skip: false }
      case 'playlist': {
        const sub = it.subscription != null ? `購読中（${it.subscription.albumartist} / ${it.subscription.album}）。` : ''
        const p = probes.get(url)
        if (p == null || p.state === 'loading') {
          return { url, kindLabel, tone: 'pending', badge: '再生リスト', detail: `${sub}中身を調べている…`, skip: false }
        }
        if (p.state === 'error') {
          return { url, kindLabel, tone: 'new', badge: '再生リスト', detail: `${sub}中身を調べられない（${p.message}）。投入すると展開時に分かる`, skip: false }
        }
        if (p.info.new === 0 && !p.info.truncated) {
          return { url, kindLabel, tone: 'skip', badge: '新規なし', detail: `${sub}${playlistDetail(p.info)}`, skip: true }
        }
        return { url, kindLabel, tone: 'new', badge: '再生リスト', detail: `${sub}${playlistDetail(p.info)}`, skip: false }
      }
    }
  })
}

/** 行の要約（「4 行（動画 2・再生リスト 1・取れない 1）」） */
export function urlRowsSummary(urls: readonly string[], lookup: ReadonlyMap<string, LookupItem>): string {
  if (urls.length === 0) return ''
  const n: Record<UrlKind, number> = { video: 0, playlist: 0, other: 0, invalid: 0 }
  for (const u of urls) {
    const k = lookup.get(u)?.kind
    if (k != null) n[k] += 1
  }
  const parts: string[] = []
  if (n.video > 0) parts.push(`動画 ${n.video}`)
  if (n.playlist > 0) parts.push(`再生リスト ${n.playlist}`)
  if (n.other > 0) parts.push(`その他 ${n.other}`)
  if (n.invalid > 0) parts.push(`取れない ${n.invalid}`)
  return `${urls.length} 行${parts.length > 0 ? `（${parts.join('・')}）` : ''}`
}

// ---------------------------------------------------------------- ③ ④ の行方

/** 今回の投入で出たジョブ: 投入で返ったジョブと、その再生リストの展開で増えた子（payload の
 *  `parent_job_id`。D-87）。id の古い順 */
export function sessionJobs(jobs: readonly Job[] | null, roots: readonly number[]): Job[] {
  if (jobs == null || roots.length === 0) return []
  const ids = new Set(roots)
  return jobs
    .filter((j) => {
      const parent = j.payload?.parent_job_id
      return ids.has(j.id) || (typeof parent === 'number' && ids.has(parent))
    })
    .sort((a, b) => a.id - b.id)
}

/** ytdl の結果 note から、Inbox に置いたファイルのディレクトリ（= Inbox の件）。置いていなければ null */
export function stagedDir(j: Pick<Job, 'state' | 'note'>): string | null {
  const prefix = 'Inbox に置いた: '
  if (j.state !== 'done' || j.note == null || !j.note.startsWith(prefix)) return null
  const path = j.note.slice(prefix.length).trim()
  const i = path.lastIndexOf('/')
  return i < 0 ? '' : path.slice(0, i)
}

/** Inbox の件ごとの曲数（置いた順） */
export function stagedDirs(jobs: readonly Job[]): { dir: string; count: number }[] {
  const out = new Map<string, number>()
  for (const j of jobs) {
    const d = stagedDir(j)
    if (d != null) out.set(d, (out.get(d) ?? 0) + 1)
  }
  return [...out.entries()].map(([dir, count]) => ({ dir, count }))
}

/** ジョブが終わった（done / failed / cancelled） */
export function isFinished(j: Pick<Job, 'state'>): boolean {
  return j.state === 'done' || j.state === 'failed' || j.state === 'cancelled'
}
