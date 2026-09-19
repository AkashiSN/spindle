// CD 取り込み（SPEC §7.2 / §12.6、P2-3）: 照会結果の型と、表示用の純粋な整形

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

export type LookupResponse = {
  discid: string
  mb_toc: string
  accuraterip_id: string
  ctdb_toc_id: string
  exact: boolean
  candidates: ReleaseCandidate[]
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
  if (n === 0) return r.exact ? 'DiscID は登録済みだが候補が無い' : 'MusicBrainz に見つからない（手入力へ。P2-4）'
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
