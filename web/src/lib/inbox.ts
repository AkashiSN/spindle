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
  /** 空ならアルバムアーティスト。keep_artists が true なら表示用（ファイルの ARTIST の全値を "; " で結合） */
  artist: string
  /**
   * ファイルの ARTIST をそのまま保つ（配置で触れない。P4-4、D-70）。提案は多値なら true。
   * null / 無しはこの欄が無かった旧下書き（サーバは「多値で artist が先頭値のままなら保つ」と解釈）
   */
  keep_artists?: boolean | null
}

export type InboxDraft = {
  /** 配置先の category（統制語彙の名前）。null なら _Unsorted */
  category: string | null
  albumartist: string
  album: string
  /** YYYY[-MM[-DD]] */
  date: string | null
  tracks: DraftTrack[]
  /** album gain を計算する album にする（D-74）。既定 false。追記先があればその現在値を上書きする */
  album_gain: boolean
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
  /** サイドカー spindle-inbox.json の項（ダウンローダが置いた件。D-70）。手で置いた件は null */
  source: InboxSource | null
}

/** ダウンローダの判定（spindle-inbox.json の 1 項） */
export type InboxSource = {
  source: string
  url: string | null
  channel: string | null
  /** `ok` か、判定できなかった reason（unmatched / unknown_channel / …） */
  verdict: string
  /** 判定できなかった理由（参照実装ならルールの足し方）。ok なら null */
  message: string | null
}

/** 追記先の既存 album（GET /api/inbox の destination。D-70） */
export type InboxDestination = {
  album_id: number
  album: string | null
  track_count: number
  max_track_no: number
  /** 追記先の album gain の属性（チェックボックスの初期値。D-74） */
  album_gain: boolean
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
  /** 下書きの category / albumartist / album から引いた追記先。無ければ null */
  destination: InboxDestination | null
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
  return {
    rel_path: t.rel_path,
    disc_no: t.disc_no,
    track_no: t.track_no,
    title: t.title,
    artist: t.artist,
    keep_artists: t.keep_artists === true,
  }
}

/** 承認画面が ARTIST の全値を見せるときの区切り（サーバの ARTIST_JOIN と同じ。D-70） */
export const ARTIST_JOIN = '; '

/** ファイルの ARTIST の全値（trim 済み・空は除く。出現順） */
export function artistValues(file: Pick<InboxFile, 'tags'>): string[] {
  return file.tags
    .filter(([k]) => k === 'ARTIST')
    .map(([, v]) => v.trim())
    .filter((v) => v !== '')
}

/**
 * トラックの keep_artists の初期値。ファイルが多値でなければ常に false（保存値が true でも戻す）。
 * 多値なら、保存値が無ければ true（提案）、保存値が boolean ならそれ、旧下書き（null）は
 * 「artist が先頭値のまま」ならサーバが保つと解釈するのと同じ判定
 */
export function keepArtistsFor(file: Pick<InboxFile, 'tags'> | undefined, saved: DraftTrack | null): boolean {
  const values = file == null ? [] : artistValues(file)
  if (values.length <= 1) return false
  if (saved == null) return true
  if (typeof saved.keep_artists === 'boolean') return saved.keep_artists
  return saved.artist.trim() === values[0]
}

/** ファイルの代表画像（PICTURE の先頭。走査が front cover 優先で並べる）の sha256。無ければ null */
export function pictureOf(file: Pick<InboxFile, 'tags'>): string | null {
  const v = file.tags.find(([k]) => k === 'PICTURE')?.[1]
  if (v == null) return null
  const i = v.indexOf(':')
  const hash = i < 0 ? v : v.slice(i + 1)
  return hash === '' ? null : hash
}

/** 件の見出しに出す画像（各ファイルの代表の最頻。同数なら先に現れたもの）。目安の要約で、配置後の代表とは限らない */
export function itemCover(item: Pick<InboxItem, 'tracks'>): string | null {
  const counts = new Map<string, number>()
  for (const f of item.tracks) {
    const h = pictureOf(f)
    if (h != null) counts.set(h, (counts.get(h) ?? 0) + 1)
  }
  let best: string | null = null
  let max = 0
  for (const [h, n] of counts) {
    if (n > max) {
      best = h
      max = n
    }
  }
  return best
}

export function artworkUrl(itemId: number, hash: string): string {
  return `/api/inbox/${itemId}/artwork/${hash}`
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
  const fileByKey = new Map(item.tracks.map((f) => [pathKey(f.rel_path), f]))
  if (saved == null) {
    // album gain の初期値は追記先の現在値（無ければ off。D-74）
    return {
      ...p,
      tracks: p.tracks.map((t) => ({
        ...cloneTrack(t),
        keep_artists: keepArtistsFor(fileByKey.get(pathKey(t.rel_path)), null),
      })),
      album_gain: item.destination?.album_gain ?? false,
    }
  }
  const byKey = new Map(saved.tracks.map((t) => [pathKey(t.rel_path), t]))
  return {
    category: saved.category,
    albumartist: saved.albumartist,
    album: saved.album,
    date: saved.date,
    tracks: p.tracks.map((t) => {
      const s = byKey.get(pathKey(t.rel_path))
      const file = fileByKey.get(pathKey(t.rel_path))
      return s == null
        ? { ...cloneTrack(t), keep_artists: keepArtistsFor(file, null) }
        : { ...cloneTrack(s), rel_path: t.rel_path, keep_artists: keepArtistsFor(file, s) }
    }),
    album_gain: saved.album_gain,
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
      keep_artists: t.keep_artists === true,
    })),
    album_gain: d.album_gain,
  }
}

/** 下書きの編集ができる状態か（承認 / 却下の対象になる状態） */
export function isEditable(state: InboxState): boolean {
  return state === 'pending' || state === 'failed'
}

/** 追記先の表示（D-70）。無ければ null */
export function destinationLabel(d: InboxDestination | null): string | null {
  if (d == null) return null
  const name = d.album == null ? '既存のアルバム' : `既存の『${d.album}』`
  return `宛先: ${name}（${d.track_count} 曲）に追加。番号は ${d.max_track_no + 1} から`
}

/** 判定バッジの文言（D-70） */
export function verdictLabel(s: InboxSource): { text: string; ok: boolean } {
  return s.verdict === 'ok' ? { text: '判定済み', ok: true } : { text: `未判定（${s.verdict}）`, ok: false }
}

/** 操作タブの「YouTube」の入力（1 行 1 URL）を URL の配列にする。空行を落とし、重複は 1 つ */
export function parseUrlLines(text: string): string[] {
  const out: string[] = []
  for (const line of text.split(/\r?\n/)) {
    const u = line.trim()
    if (u !== '' && !out.includes(u)) out.push(u)
  }
  return out
}
