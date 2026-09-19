// トラックリスト貼り付けの行解析（SPEC §7.2、D-21 / D-65、P2-4）。通販ページや Wikipedia から
// コピーしたテキストを 1 行 1 トラックとして、番号・タイトル・アーティスト・長さに割り付ける。
// 表示は即座にプレビューし、間違いはフォームで直す前提なので、判断は決め打ちで良い（推測して当てに
// 行かない）。規則:
//   - 行頭の番号: `1.` `01` `1)` `[01]` `Track 1:` `M-1` `#1` と全角。1..=99 だけ（年や `100 Years` は番号にしない）
//   - 行末の時間: `4:32` `(3:05)` `[12:00]` `1:02:03`。秒は 00..59。数字とコロンは全角も受ける
//   - アーティストの区切り: `/`（全角 `／` は空白無しでも）> `|` `｜` > ` - ` ` – ` ` — `。
//     既定は「タイトル / アーティスト」、artistFirst で逆。区切りは**アーティスト側の端**で切る
//     （タイトル内の ` - Remix - ` を残す）。端にぶら下がった区切り（`Title／`、`A - B -`）は落とす
//   - タブがあれば表の貼り付け: 数字だけの列は番号、時間の列は長さ、残りをタイトル・アーティストの順
//   - 空行と `Disc 1` `トラックリスト` `収録曲` のような見出しは飛ばす（警告に残す）
//   - 番号の無い行は前の行 + 1（先頭は 1）。重複・連番でない番号は警告

export type ParsedTrack = {
  no: number
  title: string
  artist: string | null
  length_ms: number | null
}

export type TracklistOptions = {
  /** 区切りの左がアーティスト（`アーティスト / タイトル`） */
  artistFirst: boolean
}

export type ParsedTracklist = {
  tracks: ParsedTrack[]
  warnings: string[]
}

const FULLWIDTH_DIGITS = '０１２３４５６７８９'

/** 全角数字を含む数字列を数値に */
function digitsToNumber(s: string): number {
  let out = ''
  for (const ch of s) {
    const i = FULLWIDTH_DIGITS.indexOf(ch)
    out += i >= 0 ? String(i) : ch
  }
  return Number(out)
}

// 行頭の番号: 任意の接頭辞（Track / Tr. / M- / # / No.）+ 数字 1..3 桁 + 区切り（. ． : ： ) ） ] 】 - 、 空白）
const LEADING_NUMBER =
  /^(?:\[|【|（|\()?(?:tr(?:ack)?\.?\s*|m[-\s]?|#\s*|no\.?\s*)?([0-9０-９]{1,3})(?:\]|】|）|\))?(?:\s*[.．:：\-–、]\s*|\s+)(.*)$/iu

/** 行頭の番号を外す。1..=99 でなければ番号無し */
function splitLeadingNumber(line: string): { no: number | null; rest: string } {
  const m = LEADING_NUMBER.exec(line)
  if (!m) return { no: null, rest: line }
  const no = digitsToNumber(m[1]!)
  if (no < 1 || no > 99) return { no: null, rest: line }
  const rest = m[2]!.trim()
  if (rest === '') return { no: null, rest: line }
  return { no, rest }
}

// 時間: `m:ss` / `mm:ss` / `h:mm:ss`。数字とコロンは全角も受ける（判定と数値化だけ。タイトルは触らない）
const DURATION = /^(?:([0-9０-９]{1,2})[:：])?([0-9０-９]{1,3})[:：]([0-5０-５][0-9０-９])$/u
const TRAILING_DURATION = /\s*[(（[]?\s*((?:[0-9０-９]{1,2}[:：])?[0-9０-９]{1,3}[:：][0-5０-５][0-9０-９])\s*[)）\]]?$/u

/** `m:ss` / `h:mm:ss` をミリ秒に。時間に見えなければ null */
function parseDuration(s: string): number | null {
  const m = DURATION.exec(s.trim())
  if (!m) return null
  const h = m[1] != null ? digitsToNumber(m[1]) : 0
  return ((h * 60 + digitsToNumber(m[2]!)) * 60 + digitsToNumber(m[3]!)) * 1000
}

/** 行末の時間を外す */
function splitTrailingDuration(line: string): { length_ms: number | null; rest: string } {
  const m = TRAILING_DURATION.exec(line)
  if (!m || m.index === 0) return { length_ms: null, rest: line }
  const length_ms = parseDuration(m[1]!)
  if (length_ms == null) return { length_ms: null, rest: line }
  return { length_ms, rest: line.slice(0, m.index).trim() }
}

// アーティストの区切り。優先順に試し、最初に見つかった種類で切る
const SEPARATORS: RegExp[] = [/\s*／\s*|\s+\/\s+/g, /\s*｜\s*|\s+\|\s+/g, /\s+[-–—]\s+/g]

const EDGE_SEPARATOR_HEAD = /^(?:[／｜]\s*|[/|\-–—]\s+)+/u
const EDGE_SEPARATOR_TAIL = /(?:\s*[／｜]|\s+[/|\-–—])+$/u

/**
 * タイトルとアーティストに分ける。区切りはアーティスト側の端（title-first なら最後、artist-first なら最初）
 * から内側へ見て、両側が空にならない最初のもの（`Title／Artist／` の末尾のような空の区切りは飛ばす）
 */
function splitArtist(raw: string, artistFirst: boolean): { title: string; artist: string | null } {
  // 端にぶら下がった区切り（`Title／`、`A - B -`）は貼り付けの残骸なので落とす。空白無しの `-` は残す（`-Intro-`）
  const text = raw.replace(EDGE_SEPARATOR_HEAD, '').replace(EDGE_SEPARATOR_TAIL, '')
  for (const re of SEPARATORS) {
    const matches = [...text.matchAll(re)]
    if (!artistFirst) matches.reverse()
    for (const m of matches) {
      const left = text.slice(0, m.index).trim()
      const right = text.slice(m.index + m[0].length).trim()
      if (left === '' || right === '') continue
      return artistFirst ? { title: right, artist: left } : { title: left, artist: right }
    }
  }
  return { title: text, artist: null }
}

const HEADING = /^(?:(?:disc|disk|cd|ディスク)\s*[0-9０-９]{1,2}|track\s*list|tracklist|トラックリスト|収録曲|曲目)\s*[:：]?\s*$/iu

type Fields = { no: number | null; title: string; artist: string | null; length_ms: number | null }

/** 1 行を分解（番号は未割り当てなら null） */
function parseLine(line: string, artistFirst: boolean): Fields {
  if (line.includes('\t')) return parseColumns(line.split('\t'), artistFirst)
  const { no, rest } = splitLeadingNumber(line)
  const { length_ms, rest: body } = splitTrailingDuration(rest)
  return { no, ...splitArtist(body, artistFirst), length_ms }
}

/** タブ区切りの列: 数字だけの列は番号、時間の列は長さ、残りはタイトル・アーティスト（列内では区切り文字を見ない） */
function parseColumns(cols: string[], artistFirst: boolean): Fields {
  let no: number | null = null
  let length_ms: number | null = null
  const texts: string[] = []
  for (const raw of cols) {
    const c = raw.trim()
    if (c === '') continue
    if (no == null && /^[0-9０-９]{1,2}$/u.test(c) && texts.length === 0) {
      const n = digitsToNumber(c)
      if (n >= 1 && n <= 99) {
        no = n
        continue
      }
    }
    if (length_ms == null) {
      const d = parseDuration(c)
      if (d != null) {
        length_ms = d
        continue
      }
    }
    texts.push(c)
  }
  let title = texts[0] ?? ''
  let artist = texts[1] ?? null
  if (artistFirst && texts.length >= 2) [artist, title] = [texts[0]!, texts[1]!]
  // 番号の列が無ければタイトル列の行頭番号を見る（"1. Title<TAB>4:32" の形）
  if (no == null) {
    const s = splitLeadingNumber(title)
    no = s.no
    title = s.rest
  }
  return { no, title, artist, length_ms }
}

export function parseTracklist(text: string, opts?: Partial<TracklistOptions>): ParsedTracklist {
  const artistFirst = opts?.artistFirst ?? false
  const tracks: ParsedTrack[] = []
  const warnings: string[] = []
  const skipped: string[] = []
  let next = 1
  for (const raw of text.split(/\r?\n/)) {
    const line = raw.trim()
    if (line === '') continue
    if (HEADING.test(line)) {
      skipped.push(line)
      continue
    }
    const f = parseLine(line, artistFirst)
    const no = f.no ?? next
    next = no + 1
    tracks.push({ no, title: f.title, artist: f.artist, length_ms: f.length_ms })
  }
  if (skipped.length > 0) warnings.push(`見出しとして飛ばした行: ${skipped.join(' / ')}`)
  const seen = new Set<number>()
  const dup = new Set<number>()
  for (const t of tracks) (seen.has(t.no) ? dup : seen).add(t.no)
  if (dup.size > 0) warnings.push(`番号が重複: ${[...dup].sort((a, b) => a - b).join(', ')}`)
  if (tracks.length > 0 && tracks.some((t, i) => t.no !== i + 1)) warnings.push('番号が 1 からの連番ではない')
  return { tracks, warnings }
}
