// Inbox の承認キュー（SPEC §7.8、D-68）の純粋ロジック。件 = 音声ファイルのあるディレクトリ。
// 下書き（InboxDraft）はアルバム単位 + トラック単位の補正で、承認時にサーバへ送り、配置時にタグへ書く。
// 検証はサーバ（import::inbox::InboxDraft::problems）と同じ規則・同じ文言

export type InboxState = 'pending' | 'approved' | 'placing' | 'placed' | 'failed' | 'rejected'

export type DraftTrack = {
  /** Inbox 相対（件のファイルと 1:1） */
  rel_path: string
  disc_no: number
  track_no: number
  title: string
  /** 空ならアルバムアーティスト */
  artist: string
}

export type InboxDraft = {
  /** 配置先の category（統制語彙の名前）。null なら _Unsorted */
  category: string | null
  albumartist: string
  album: string
  /** YYYY[-MM[-DD]] */
  date: string | null
  tracks: DraftTrack[]
}

export type InboxFile = {
  rel_path: string
  inode: number
  size: number
  mtime_ns: number
  ctime_ns: number
  codec: string
  lossless: boolean
  sample_rate: number | null
  bit_depth: number | null
  channels: number | null
  duration_ms: number | null
  /** 正規化済みタグ（キーは大文字。多値は反復） */
  tags: Array<[string, string]>
}

/** GET /api/inbox の 1 件 */
export type InboxItem = {
  id: number
  rel_dir: string
  state: InboxState
  detected_at: number
  seen_at: number
  approved_at: number | null
  /** 承認時に保存した下書き（失敗 / 再開後もそのまま残る） */
  draft: InboxDraft | null
  error: string | null
  placed_album_id: number | null
  placed_at: number | null
  tracks: InboxFile[]
  /** タグから作った提案（毎回作り直される） */
  proposal: InboxDraft
  warnings: string[]
}

export const STATE_LABELS: Record<InboxState, string> = {
  pending: '未処理',
  approved: '承認済み（配置待ち）',
  placing: '配置中',
  placed: '配置済み',
  failed: '失敗',
  rejected: '却下',
}

export function stateLabel(state: InboxState): string {
  return STATE_LABELS[state]
}

export function itemTitle(item: Pick<InboxItem, 'rel_dir'>): string {
  return item.rel_dir === '' ? '(Inbox 直下)' : item.rel_dir
}

/** 件に含まれるコーデックの集合（`flac / wav`） */
export function codecSummary(files: Array<Pick<InboxFile, 'codec'>>): string {
  return [...new Set(files.map((f) => f.codec))].sort().join(' / ')
}

function cloneTrack(t: DraftTrack): DraftTrack {
  return { rel_path: t.rel_path, disc_no: t.disc_no, track_no: t.track_no, title: t.title, artist: t.artist }
}

/** パス照合の鍵（サーバの canonical_key の近似: NFD + casefold）。表示には使わない */
function pathKey(s: string): string {
  return s.normalize('NFD').toLowerCase().normalize('NFD')
}

/**
 * フォームの初期値。保存済みの下書きがあればアルバム単位の値とトラックの補正をそれから取り、
 * 無ければ提案。トラックの並びは常に現在のファイル（提案の順）に合わせる: 走査で増えたファイルは
 * 提案の値で足し、消えたファイルの行は落とす（そのまま送ると「件に無いファイル」で弾かれる）
 */
export function draftFrom(item: InboxItem): InboxDraft {
  const p = item.proposal
  const saved = item.draft
  if (saved == null) {
    return { ...p, tracks: p.tracks.map(cloneTrack) }
  }
  const byKey = new Map(saved.tracks.map((t) => [pathKey(t.rel_path), t]))
  return {
    category: saved.category,
    albumartist: saved.albumartist,
    album: saved.album,
    date: saved.date,
    tracks: p.tracks.map((t) => {
      const s = byKey.get(pathKey(t.rel_path))
      return s == null ? cloneTrack(t) : { ...cloneTrack(s), rel_path: t.rel_path }
    }),
  }
}

/** YYYY[-MM[-DD]] か（サーバの is_valid_date と同じ） */
export function isValidDate(s: string): boolean {
  const parts = s.split('-')
  if (parts.length === 0 || parts.length > 3) return false
  const digits = (p: string, n: number) => p.length === n && /^[0-9]+$/.test(p)
  if (!digits(parts[0], 4)) return false
  if (parts.length >= 2) {
    const m = Number.parseInt(parts[1], 10)
    if (!digits(parts[1], 2) || m < 1 || m > 12) return false
  }
  if (parts.length === 3) {
    const d = Number.parseInt(parts[2], 10)
    if (!digits(parts[2], 2) || d < 1 || d > 31) return false
  }
  return true
}

/**
 * 下書きの問題を全部返す（空なら承認できる）。`files` は件のファイルの rel_path。
 * 規則と文言はサーバの InboxDraft::problems と揃える（サーバは最初の 1 件を 400 で返す）
 */
export function validateDraft(d: InboxDraft, files: string[]): string[] {
  const out: string[] = []
  if (d.album.trim() === '') out.push('アルバム名が空')
  if (d.albumartist.trim() === '') out.push('アルバムアーティストが空')
  if (d.category != null && d.category.trim() === '') out.push('category が空')
  if (d.date != null && !isValidDate(d.date.trim())) {
    out.push(`日付の形が不正: ${d.date}（YYYY / YYYY-MM / YYYY-MM-DD）`)
  }
  const known = new Set(files.map(pathKey))
  const seen = new Set<string>()
  const numbers = new Set<string>()
  for (const t of d.tracks) {
    const key = pathKey(t.rel_path)
    if (!known.has(key)) out.push(`件に無いファイル: ${t.rel_path}`)
    if (seen.has(key)) out.push(`下書きに同じファイルが 2 回: ${t.rel_path}`)
    seen.add(key)
    if (t.title.trim() === '') out.push(`タイトルが空: ${t.rel_path}`)
    if (!(t.disc_no >= 1) || !(t.track_no >= 1)) {
      out.push(`トラック番号 / ディスク番号は 1 以上: ${t.rel_path}`)
    } else {
      const n = `${t.disc_no}/${t.track_no}`
      if (numbers.has(n)) out.push(`番号が重複: disc ${t.disc_no} track ${t.track_no}`)
      numbers.add(n)
    }
  }
  for (const f of files) {
    if (!seen.has(pathKey(f))) out.push(`下書きに無いファイル: ${f}`)
  }
  return out
}

/** 送信用に整える: 前後の空白を落とし、空の date / category は null */
export function draftForSubmit(d: InboxDraft): InboxDraft {
  const opt = (s: string | null) => {
    const v = s?.trim() ?? ''
    return v === '' ? null : v
  }
  return {
    category: opt(d.category),
    albumartist: d.albumartist.trim(),
    album: d.album.trim(),
    date: opt(d.date),
    tracks: d.tracks.map((t) => ({
      rel_path: t.rel_path,
      disc_no: t.disc_no,
      track_no: t.track_no,
      title: t.title.trim(),
      artist: t.artist.trim(),
    })),
  }
}

/** 下書きの編集ができる状態か（承認 / 却下の対象になる状態） */
export function isEditable(state: InboxState): boolean {
  return state === 'pending' || state === 'failed'
}
