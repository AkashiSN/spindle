// CD 取り込み（SPEC §7.2 / §12.6、P2-3 / P2-4 / P4-20）: 照会結果の型と表示用の整形、取り込む内容
// （候補を写した 1 つの下書き）と、その確定形（吸い出し P2-5 の入力）。
//
// **CD 画面からは直せない**（P4-20 追記）。候補を選ぶ / 写す範囲を変える /「どれも違う」だけで、
// 補正とトラックリスト貼り付けは Inbox の承認画面（取り込んだものは Inbox を通る。D-67 追記）。
// P2-5 に渡す契約は**名前が空でもよい**（空名の盤は Inbox で名前を入れる。D-67 追記）。

import { formatDuration } from './format'
export type TrackCandidate = {
  number: string
  position: number
  title: string
  artist: string
  length_ms: number | null
  recording_id: string
  track_id: string
  isrcs: string[]
}

/** 候補が出てきた経路（強い順。サーバの `MatchedBy`） */
export type MatchedBy = 'discid' | 'release' | 'isrc' | 'barcode' | 'toc'

export const MATCHED_BY_ORDER: MatchedBy[] = ['discid', 'release', 'isrc', 'barcode', 'toc']

export const MATCHED_BY_LABELS: Record<MatchedBy, string> = {
  discid: 'DiscID 一致',
  release: '指定',
  isrc: 'ISRC',
  barcode: 'バーコード',
  toc: 'TOC 近似',
}

/** リリースに入っている 1 枚（候補の medium かどうかに関わらず並ぶ） */
export type MediumInfo = {
  position: number
  /** `CD` / `Blu-ray` / `Digital Media` など。MB に無ければ null */
  format: string | null
  track_count: number
}

export type ReleaseCandidate = {
  release_id: string
  release_group_id: string | null
  title: string
  artist: string
  date: string | null
  country: string | null
  status: string | null
  barcode: string | null
  disambiguation: string | null
  labels: Array<[string, string | null]>
  exact: boolean
  /** どの経路で出てきたか（強い順） */
  matched_by: MatchedBy[]
  /** リリース全体の収録構成（DVD 付き / BD 付き / デジタルの区別に使う） */
  media: MediumInfo[]
  medium_position: number
  medium_count: number
  medium_title: string | null
  format: string | null
  tracks: TrackCandidate[]
}

/** TOC の音声トラック（候補が無くても表の行数と長さの元になる） */
export type TocTrackInfo = {
  number: number
  length_ms: number
}

/** 照会が止まった段（サーバの `LookupStage`）。上の段で候補が残れば下は引かない（D-64 追記 4） */
export type LookupStage = 'discid' | 'ids' | 'toc'

export type LookupResponse = {
  discid: string
  mb_toc: string
  accuraterip_id: string
  ctdb_toc_id: string
  exact: boolean
  /** どの段で止まったか */
  stage: LookupStage
  /** まだ引いていない段がある（「さらに広げて探す」を出す） */
  can_widen: boolean
  candidates: ReleaseCandidate[]
  /** 候補に入れられなかった理由（指定リリースが読めない・トラック数が合わない） */
  notes: string[]
  tracks: TocTrackInfo[]
}

/** 候補のバッジ: 経路を強い順に並べる */
export function matchedByLabel(c: ReleaseCandidate): string {
  return MATCHED_BY_ORDER.filter((m) => c.matched_by.includes(m))
    .map((m) => MATCHED_BY_LABELS[m])
    .join(' / ')
}

/**
 * DiscID の登録を案内するか: 照会が DiscID で当たっておらず、fuzzy に混ざった DiscID 持ちの候補も無い
 * （= 本当に未登録）ときだけ。exact 候補があるのに案内すると二重登録を促す
 */
export function offersDiscidSubmission(r: LookupResponse): boolean {
  return !r.exact && !r.candidates.some((c) => c.exact)
}

/**
 * MusicBrainz に DiscID を登録するページ（libdiscid の submission URL と同じ形。登録はブラウザで本人が行う）。
 * `mbToc` は MB 形式 `先頭 末尾 リードアウト+150 各オフセット+150…`（空白は + に）
 */
export function discidSubmissionUrl(discid: string, mbToc: string): string {
  const parts = mbToc.trim().split(/\s+/)
  const tracks = Math.max(0, parts.length - 3)
  return `https://musicbrainz.org/cdtoc/attach?id=${encodeURIComponent(discid)}&tracks=${tracks}&toc=${parts.join('+')}`
}

/**
 * 貼り付けた TOC を API に渡す形に整える。CTDB 形式（`0:13915:…:leadout`）と MusicBrainz 形式
 * （`1 12 leadout+150 offsets…`）はそのまま。cdrdao / cdrecord の `-toc` 出力（`track: 1 lba: 0 …` と
 * `track:lout lba: 95312`）は LBA を拾って CTDB 形式にする（データトラックは control の bit 2 で
 * 見分ける: `control: 4` / `6`）
 */
export function normalizeTocInput(text: string): string {
  const t = text.trim()
  if (!/track:\s*(\d+|lout)/i.test(t)) return t
  const starts: string[] = []
  let leadout: string | null = null
  for (const line of t.split(/\r?\n/)) {
    const m = /track:\s*(\d+|lout)\s+lba:\s*(\d+)(?:.*control:\s*(\d+))?/i.exec(line)
    if (!m) continue
    const [, no, lba, control] = m
    if (no!.toLowerCase() === 'lout') {
      leadout = lba!
      continue
    }
    const isData = control != null && (Number(control) & 4) !== 0
    starts.push(isData ? `-${lba}` : lba!)
  }
  if (starts.length === 0 || leadout == null) return t
  return [...starts, leadout].join(':')
}

/** 候補の一行要約: 日付 · 国 · レーベル カタログ番号 · 形式 n/m · 注記 */
export function candidateSummary(c: ReleaseCandidate): string {
  const parts: string[] = []
  if (c.date) parts.push(c.date)
  if (c.country) parts.push(c.country)
  for (const [label, catalog] of c.labels) parts.push(catalog ? `${label} ${catalog}` : label)
  if (c.barcode) parts.push(`JAN/UPC ${c.barcode}`)
  // 形式と何枚目かは mediaSummary が出す（リリース全体の構成も含めて見せる）
  if (c.status && c.status !== 'Official') parts.push(c.status)
  if (c.disambiguation) parts.push(c.disambiguation)
  return parts.join(' · ')
}

/** MusicBrainz のリリースのページ（候補の出どころ） */
export function releaseUrl(c: ReleaseCandidate): string {
  return `https://musicbrainz.org/release/${c.release_id}`
}

/**
 * CD として吸い出せる形式（MusicBrainz の Release/Format の名前。括弧の注記を外した基本形で見る）。
 * `Hybrid SACD` と `DualDisc` は CD 面を持つ形式なので含める
 */
const CD_READABLE = new Set([
  'cd',
  'cd-r',
  '8cm cd',
  'enhanced cd',
  'mixed mode cd',
  'hdcd',
  'copy control cd',
  'shm-cd',
  'blu-spec cd',
  'hqcd',
  'uhqcd',
  'dts cd',
  'cd+g',
  '8cm cd+g',
  'minimax cd',
  'hybrid sacd',
  'dualdisc',
  'dvdplus',
])

/** 名前に CD が入っていても音声 CD として吸い出せない形式 */
const NOT_CD_NAMES = new Set(['data cd', 'cd-rom', 'vcd', 'svcd', 'cdv'])

/** CD 系でない系統（形式名の族。`12" Vinyl` のような派生も拾う） */
const NOT_CD_FAMILY = /DVD|Blu-?ray|HD-?DVD|SACD|Digital Media|Vinyl|Cassette|Shellac|Reel|MiniDisc|\bDAT\b|Wax/i

/** 複合ディスクの注記: CD の層・面ならそこを吸える、他の層・面なら吸えない */
const CD_LAYER = /\bCD (layer|side)\b/i

/** 括弧の注記（`Hybrid SACD (CD layer)` の `(CD layer)`）を外した基本形 */
function baseFormat(format: string): string {
  return format
    .replace(/\s*\([^)]*\)\s*$/, '')
    .trim()
    .toLowerCase()
}

/**
 * 吸い出せる medium か（CD 系）。MusicBrainz の管理された形式名で判定する。
 * `Hybrid SACD` / `DualDisc` / `DVDplus` は CD 面を持つので CD 扱い（`(SACD layer)` や
 * `(DVD-Video side)` の注記が付いていればその面なので除く）。`Data CD` のような音声でない CD は除く。
 * 形式が分からないものは隠さない（MB の登録漏れで CD のことがある）
 */
export function isCdMedium(c: ReleaseCandidate): boolean {
  const f = c.format
  if (f == null) return true
  // 注記が「CD の層 / 面」なら、基本形が何であれ吸える
  if (CD_LAYER.test(f)) return true
  const base = baseFormat(f)
  const qualified = base !== f.trim().toLowerCase()
  // 注記付きで CD 面でないもの（`Hybrid SACD (SACD layer)` / `DualDisc (DVD-Video side)`）は除く
  if (qualified && CD_READABLE.has(base)) return false
  if (CD_READABLE.has(base)) return true
  if (NOT_CD_NAMES.has(base)) return false
  if (NOT_CD_FAMILY.test(base)) return false
  return /CD/i.test(base)
}

/** CD 系の候補とそれ以外に分ける（それぞれ元の順序を保つ） */
export function splitByMedium(candidates: ReleaseCandidate[]): {
  cd: ReleaseCandidate[]
  other: ReleaseCandidate[]
} {
  return {
    cd: candidates.filter(isCdMedium),
    other: candidates.filter((c) => !isCdMedium(c)),
  }
}

/**
 * リリースの収録構成と、いま見ている枚。`CD + Blu-ray の 1 枚目`、`CD 2 枚組の 1 枚目`、`CD`。
 * 同じ曲でも DVD 付き / BD 付き / デジタルで別リリースになるので、これが選別の決め手になる
 */
export function mediaSummary(c: ReleaseCandidate): string {
  const formats = c.media.length > 0 ? c.media.map((m) => m.format ?? '形式不明') : [c.format ?? '形式不明']
  // 形式ごとの枚数を出てきた順に（CD 2 枚 + Blu-ray と CD + Blu-ray は別の版）
  const counts = new Map<string, number>()
  for (const f of formats) counts.set(f, (counts.get(f) ?? 0) + 1)
  const single = counts.size === 1
  const all =
    single && formats.length === 1
      ? formats[0]!
      : single
        ? `${[...counts.keys()][0]} ${formats.length} 枚組`
        : [...counts].map(([f, n]) => (n > 1 ? `${f} ${n} 枚` : f)).join(' + ')
  if (c.medium_count <= 1) return all
  // 「CD 2 枚組の 1 枚目」「CD + Blu-ray の 1 枚目」
  const joiner = single ? 'の' : ' の'
  return `${all}${joiner} ${c.medium_position} 枚目`
}

/** ディスク（TOC）と候補の長さの差（ms。候補に長さ不明があるか TOC が空なら null） */
export function lengthDiffMs(c: ReleaseCandidate, tocTracks: TocTrackInfo[]): number | null {
  if (tocTracks.length === 0) return null
  const total = candidateLengthMs(c)
  if (total == null) return null
  const disc = tocTracks.reduce((a, t) => a + t.length_ms, 0)
  return total - disc
}

/** 長さ差の表示。1 秒未満は 0.1 秒まで */
export function formatLengthDiff(ms: number): string {
  if (ms === 0) return '長さ一致'
  const sec = Math.abs(ms) / 1000
  const sign = ms > 0 ? '+' : '−'
  const value = sec < 10 ? sec.toFixed(1) : Math.round(sec).toString()
  return `長さ差 ${sign}${value} 秒`
}

/** 候補全体の長さ（ms 不明のトラックがあれば null） */
export function candidateLengthMs(c: ReleaseCandidate): number | null {
  let total = 0
  for (const t of c.tracks) {
    if (t.length_ms == null) return null
    total += t.length_ms
  }
  return total
}

/** 候補の 2 行目: 収録構成・日付・国・レーベル・JAN/UPC・曲数・長さ・ディスクとの長さ差（CD 画面と Inbox の引き直し） */
export function candidateDetail(c: ReleaseCandidate, tocTracks: TocTrackInfo[]): string {
  const parts = [mediaSummary(c), candidateSummary(c), `${c.tracks.length} 曲`]
  const len = candidateLengthMs(c)
  if (len != null) parts.push(formatDuration(len))
  const diff = lengthDiffMs(c, tocTracks)
  if (diff != null) parts.push(formatLengthDiff(diff))
  return parts.filter((p) => p !== '').join(' · ')
}

/**
 * 照会結果の見出し。`exact` は照会の経路（DiscID そのもので引けたか）で、TOC の fuzzy 照会でも
 * 候補側に DiscID を持つ medium が混ざることがある（D-64）ので、候補の exact も見る
 */
export function lookupHeadline(r: LookupResponse): string {
  const n = r.candidates.length
  if (n === 0)
    return r.exact
      ? 'DiscID は登録済みだが候補が無い（そのまま取り込んで Inbox で名前を入れる）'
      : 'MusicBrainz に見つからない（そのまま取り込んで Inbox で名前を入れる）'
  if (r.stage === 'discid') return `DiscID が一致: ${n} 件`
  const routes = MATCHED_BY_ORDER.filter((m) => r.candidates.some((c) => c.matched_by.includes(m)))
    .map((m) => MATCHED_BY_LABELS[m])
    .join(' / ')
  // `exact` のまま下の段へ落ちることがある（DiscID は登録済みだが曲数の合う medium が無い。
  // D-64 追記 4）。ここで「DiscID は未登録」と言うと嘘になる
  const discid = r.exact ? 'DiscID は登録済みだが曲数の合う候補が無い' : 'DiscID は未登録'
  return `候補: ${n} 件（${routes}。${discid}）`
}

/** 照会後の状態（結果・選択・エラー）。TOC を編集したら古いものを捨てる */
export type LookupOutcome = { result: LookupResponse | null; selected: number | null; error: string | null }

/** TOC の入力が変わったときの遷移: 結果・選択・エラーを消す（入力が同じなら何もしない） */
export function outcomeAfterTocEdit(prevToc: string, nextToc: string, outcome: LookupOutcome): LookupOutcome {
  if (prevToc === nextToc) return outcome
  return { result: null, selected: null, error: null }
}

/** 照会が返ったときの選択: DiscID 一致がちょうど 1 件ならそれ、それ以外は未選択 */
export function initialSelection(r: LookupResponse): number | null {
  const exact = r.candidates.map((c, i) => (c.exact ? i : -1)).filter((i) => i >= 0)
  return exact.length === 1 ? exact[0]! : null
}

// ---------------------------------------------------------------- 取り込む内容（P2-4、D-21 / D-65）

/** 候補から持ち越す MusicBrainz のトラック識別子（タグの MUSICBRAINZ_TRACKID 等。P2-8） */
export type TrackMbIds = { recording_id: string; track_id: string; isrcs: string[] }

/** フォームの 1 行。行は TOC の音声トラックと 1:1 で、番号と長さは TOC から（編集不可） */
export type DiscTrackDraft = {
  number: number
  length_ms: number
  title: string
  /** 空ならアルバムアーティスト（確定時に埋める） */
  artist: string
  mb: TrackMbIds | null
}

/** 取り込む内容。候補を写したものも空のものも同じ形（CD 画面からは直せない。直すのは Inbox） */
export type DiscDraft = {
  source: 'musicbrainz' | 'manual'
  release_id: string | null
  release_group_id: string | null
  album: string
  album_artist: string
  /** YYYY / YYYY-MM / YYYY-MM-DD。空可 */
  date: string
  label: string
  catalog_number: string
  barcode: string
  disc_no: number
  disc_count: number
  /** 配置先の category（統制語彙の名前）。null なら _Unsorted。タグには書かない（D-67） */
  category: string | null
  tracks: DiscTrackDraft[]
}

export type DiscTrackMetadata = {
  number: number
  title: string
  artist: string
  mb: TrackMbIds | null
}

/** 確定したディスクのメタデータ（ウィザードの出力。吸い出し P2-5 と配置 P2-8 が同じ形を受ける） */
export type DiscMetadata = {
  source: 'musicbrainz' | 'manual'
  release_id: string | null
  release_group_id: string | null
  album: string
  album_artist: string
  date: string | null
  label: string | null
  catalog_number: string | null
  barcode: string | null
  disc_no: number
  disc_count: number
  category: string | null
  tracks: DiscTrackMetadata[]
}

function blankRows(toc: TocTrackInfo[]): DiscTrackDraft[] {
  return toc.map((t) => ({ number: t.number, length_ms: t.length_ms, title: '', artist: '', mb: null }))
}

/** 空のフォーム（照会ゼロ件、または候補がどれも違うとき） */
export function emptyDraft(toc: TocTrackInfo[]): DiscDraft {
  return {
    source: 'manual',
    release_id: null,
    release_group_id: null,
    album: '',
    album_artist: '',
    date: '',
    label: '',
    catalog_number: '',
    barcode: '',
    disc_no: 1,
    disc_count: 1,
    category: null,
    tracks: blankRows(toc),
  }
}

/**
 * 候補をフォームに写す。行は TOC の音声トラック（候補のトラックは位置順に当て、足りなければ空行、
 * 余れば捨てる）。レーベルは先頭の 1 つ
 */
/**
 * 候補から写す範囲（D-72、P4-2）。`minimal` は盤を見分けるのに要る最小限（アルバム / アルバムアーティスト /
 * 日付 / ディスク番号・枚数 / MUSICBRAINZ_ALBUMID。DISCID と TRACKTOTAL は吸い出し時に TOC から付く）。レーベル・カタログ番号・JAN と各トラックの
 * タイトル・アーティスト・MB id・ISRC は `full` のときだけ写る。
 *
 * **CD 画面の既定は `full`**（D-72 追記 2、P4-20）。トラック表が主役になり、候補を選んだら名前が
 * 入るのが期待される画面になったため。値を手で入れたいときは `minimal` に切り替える
 */
export type CopyScope = 'minimal' | 'full'

export const COPY_SCOPE_LABELS: Record<CopyScope, string> = {
  minimal: '識別用の最小限',
  full: '全部写す（既定）',
}

export function draftFromCandidate(c: ReleaseCandidate, toc: TocTrackInfo[], scope: CopyScope): DiscDraft {
  const full = scope === 'full'
  const [label, catalog] = c.labels[0] ?? ['', null]
  return {
    source: 'musicbrainz',
    release_id: c.release_id,
    // Release Group は盤の版を特定する鍵ではないので最小限には含めない（D-72 の id は ALBUMID / DISCID）
    release_group_id: full ? c.release_group_id : null,
    album: c.title,
    album_artist: c.artist,
    date: c.date ?? '',
    label: full ? label : '',
    catalog_number: full ? (catalog ?? '') : '',
    barcode: full ? (c.barcode ?? '') : '',
    disc_no: c.medium_position,
    disc_count: c.medium_count,
    category: null,
    // minimal はトラック行を番号と長さだけの空行にする（名前は Inbox の承認画面で入れる）
    tracks: blankRows(toc).map((row, i) => {
      const t = c.tracks[i]
      if (!full || t == null) return row
      return {
        ...row,
        title: t.title,
        artist: t.artist,
        mb: { recording_id: t.recording_id, track_id: t.track_id, isrcs: t.isrcs },
      }
    }),
  }
}

const DATE = /^\d{4}(?:-\d{2}(?:-\d{2})?)?$/

/**
 * 取り込めない理由（空なら取り込める。サーバの `DiscMetadata::validate` と同じ規則）。
 * **名前は空でもよい**（D-67 追記。候補の無い盤は名前の無いまま Inbox へ置き、承認画面で直す。
 * 空のタイトルは `finalizeDraft` が `Track NN` で埋める）
 */
export function validateDraft(d: DiscDraft): string[] {
  const errors: string[] = []
  if (d.date.trim() !== '' && !DATE.test(d.date.trim())) errors.push('日付は YYYY / YYYY-MM / YYYY-MM-DD')
  if (d.disc_no > d.disc_count) errors.push(`ディスク番号 ${d.disc_no} が枚数 ${d.disc_count} を超える`)
  return errors
}

/**
 * 分からないトラックの既定の名前。表ではプレースホルダとして見せ、確定時に実値にする
 * （タイトルの分からないディスクでも完走できるように。D-21 / D-65）
 */
export function defaultTitle(number: number): string {
  return `Track ${String(number).padStart(2, '0')}`
}

function orNull(s: string): string | null {
  const t = s.trim()
  return t === '' ? null : t
}

/** 取り込みの入力（`POST /api/cd/rip`）にする。前後の空白を落とし、トラックのアーティストが空ならアルバムアーティストにする（validateDraft が通っている前提） */
export function finalizeDraft(d: DiscDraft): DiscMetadata {
  const album_artist = d.album_artist.trim()
  return {
    source: d.source,
    release_id: d.release_id,
    release_group_id: d.release_group_id,
    album: d.album.trim(),
    album_artist,
    date: orNull(d.date),
    label: orNull(d.label),
    catalog_number: orNull(d.catalog_number),
    barcode: orNull(d.barcode),
    disc_no: d.disc_no,
    disc_count: d.disc_count,
    category: orNull(d.category ?? ''),
    tracks: d.tracks.map((t) => ({
      number: t.number,
      // 空のままなら Track NN（表ではプレースホルダとして見えている）
      title: orNull(t.title) ?? defaultTitle(t.number),
      artist: orNull(t.artist) ?? album_artist,
      mb: t.mb,
    })),
  }
}

/** タグの写像の 1 行: キーと値の列。Vorbis コメントは同じキーを反復できるので多値（ISRC）はそのまま持つ */
export type TagRow = [string, string[]]

function tagRows(rows: Array<[string, string | string[] | null]>): TagRow[] {
  const out: TagRow[] = []
  for (const [k, v] of rows) {
    if (v == null) continue
    const values = (Array.isArray(v) ? v : [v]).filter((x) => x !== '')
    if (values.length > 0) out.push([k, values])
  }
  return out
}

/**
 * 確定したメタデータをタグに写す（P2-8 の配置が書くタグの写像。Picard / lofty の Vorbis コメント名）。
 * MusicBrainz の id は recording が MUSICBRAINZ_TRACKID、リリース内の track が MUSICBRAINZ_RELEASETRACKID
 * （Picard の写像。取り違えやすい）。ISRC は 1 値 1 行（`;` で繋がない）。TRACKTOTAL / MUSICBRAINZ_DISCID は
 * TOC から吸い出し時に付ける
 */
export function albumTags(m: DiscMetadata): TagRow[] {
  return tagRows([
    ['ALBUM', m.album],
    ['ALBUMARTIST', m.album_artist],
    ['DATE', m.date],
    ['LABEL', m.label],
    ['CATALOGNUMBER', m.catalog_number],
    ['BARCODE', m.barcode],
    ['DISCNUMBER', String(m.disc_no)],
    ['DISCTOTAL', String(m.disc_count)],
    ['MUSICBRAINZ_ALBUMID', m.release_id],
    ['MUSICBRAINZ_RELEASEGROUPID', m.release_group_id],
  ])
}

export function trackTags(t: DiscTrackMetadata): TagRow[] {
  return tagRows([
    ['TRACKNUMBER', String(t.number)],
    ['TITLE', t.title],
    ['ARTIST', t.artist],
    ['MUSICBRAINZ_TRACKID', t.mb?.recording_id ?? null],
    ['MUSICBRAINZ_RELEASETRACKID', t.mb?.track_id ?? null],
    ['ISRC', t.mb?.isrcs ?? null],
  ])
}
