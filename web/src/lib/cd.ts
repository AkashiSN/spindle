// CD 取り込み（SPEC §7.2 / §12.6、P2-3 / P2-4）: 照会結果の型と表示用の整形、手入力フォーム（候補を土台に
// 編集する 1 つの下書き）と、確定したメタデータ（吸い出し P2-5 / 配置 P2-8 の入力）

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
  medium_position: number
  medium_count: number
  medium_title: string | null
  format: string | null
  tracks: TrackCandidate[]
}

/** TOC の音声トラック（候補が無くても手入力フォームの行数と長さの元になる） */
export type TocTrackInfo = {
  number: number
  length_ms: number
}

export type LookupResponse = {
  discid: string
  mb_toc: string
  accuraterip_id: string
  ctdb_toc_id: string
  exact: boolean
  candidates: ReleaseCandidate[]
  tracks: TocTrackInfo[]
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
  const fmt = c.format ?? 'medium'
  parts.push(c.medium_count > 1 ? `${fmt} ${c.medium_position}/${c.medium_count}` : fmt)
  if (c.status && c.status !== 'Official') parts.push(c.status)
  if (c.disambiguation) parts.push(c.disambiguation)
  return parts.join(' · ')
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

/**
 * 照会結果の見出し。`exact` は照会の経路（DiscID そのもので引けたか）で、TOC の fuzzy 照会でも
 * 候補側に DiscID を持つ medium が混ざることがある（D-64）ので、候補の exact も見る
 */
export function lookupHeadline(r: LookupResponse): string {
  const n = r.candidates.length
  const exactCandidates = r.candidates.filter((c) => c.exact).length
  if (n === 0) return r.exact ? 'DiscID は登録済みだが候補が無い' : 'MusicBrainz に見つからない（手入力へ）'
  if (r.exact) return `DiscID が一致: ${n} 件`
  if (exactCandidates > 0) return `TOC で照会（DiscID の一致する候補 ${exactCandidates} 件を含む）: ${n} 件`
  return `TOC の近い候補（DiscID は未登録）: ${n} 件`
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

// ---------------------------------------------------------------- 手入力（P2-4、D-21 / D-65）

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

/** 編集中のフォーム。候補を写したものも空のものも同じ形で、確定すると DiscMetadata になる */
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
export function draftFromCandidate(c: ReleaseCandidate, toc: TocTrackInfo[]): DiscDraft {
  const [label, catalog] = c.labels[0] ?? ['', null]
  return {
    source: 'musicbrainz',
    release_id: c.release_id,
    release_group_id: c.release_group_id,
    album: c.title,
    album_artist: c.artist,
    date: c.date ?? '',
    label,
    catalog_number: catalog ?? '',
    barcode: c.barcode ?? '',
    disc_no: c.medium_position,
    disc_count: c.medium_count,
    category: null,
    tracks: blankRows(toc).map((row, i) => {
      const t = c.tracks[i]
      if (t == null) return row
      return {
        ...row,
        title: t.title,
        artist: t.artist,
        mb: { recording_id: t.recording_id, track_id: t.track_id, isrcs: t.isrcs },
      }
    }),
  }
}

/**
 * 貼り付けの解析結果を行に写す（番号で対応付け）。アーティストの無い行は既存の値を保つ。
 * TOC に無い番号は捨て、行数の違い・未設定の行とともに警告にする。元の draft は変えない
 */
export function applyTracklist(
  draft: DiscDraft,
  parsed: Array<{ no: number; title: string; artist: string | null }>,
): { draft: DiscDraft; warnings: string[] } {
  const warnings: string[] = []
  if (parsed.length !== draft.tracks.length) warnings.push(`貼り付けの行数 ${parsed.length} が TOC の ${draft.tracks.length} と違う`)
  const byNo = new Map(draft.tracks.map((t, i) => [t.number, i]))
  const tracks = draft.tracks.map((t) => ({ ...t }))
  const unknown: number[] = []
  const covered = new Set<number>()
  for (const p of parsed) {
    const i = byNo.get(p.no)
    if (i == null) {
      unknown.push(p.no)
      continue
    }
    covered.add(p.no)
    tracks[i]!.title = p.title
    if (p.artist != null) tracks[i]!.artist = p.artist
  }
  if (unknown.length > 0) warnings.push(`TOC に無い番号: ${unknown.join(', ')}`)
  const missing = draft.tracks.filter((t) => !covered.has(t.number)).map((t) => t.number)
  if (missing.length > 0) warnings.push(`未設定の行: ${missing.join(', ')}`)
  return { draft: { ...draft, tracks }, warnings }
}

const DATE = /^\d{4}(?:-\d{2}(?:-\d{2})?)?$/

/** 確定できない理由（空なら確定できる） */
export function validateDraft(d: DiscDraft): string[] {
  const errors: string[] = []
  if (d.album.trim() === '') errors.push('アルバム名が空')
  if (d.album_artist.trim() === '') errors.push('アルバムアーティストが空')
  const untitled = d.tracks.filter((t) => t.title.trim() === '').map((t) => t.number)
  if (untitled.length > 0) errors.push(`タイトルが空: ${untitled.join(', ')}`)
  if (d.date.trim() !== '' && !DATE.test(d.date.trim())) errors.push('日付は YYYY / YYYY-MM / YYYY-MM-DD')
  if (d.disc_no > d.disc_count) errors.push(`ディスク番号 ${d.disc_no} が枚数 ${d.disc_count} を超える`)
  return errors
}

/** 空のタイトルを `Track NN` で埋める（タイトルの分からないディスクでも完走できるように） */
export function fillEmptyTitles(d: DiscDraft): DiscDraft {
  return {
    ...d,
    tracks: d.tracks.map((t) =>
      t.title.trim() === '' ? { ...t, title: `Track ${String(t.number).padStart(2, '0')}` } : t,
    ),
  }
}

function orNull(s: string): string | null {
  const t = s.trim()
  return t === '' ? null : t
}

/** 確定。前後の空白を落とし、トラックのアーティストが空ならアルバムアーティストにする（validate が通っている前提） */
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
      title: t.title.trim(),
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
