import type { ReleaseCandidate } from './cd'
import { formatDuration } from './format'

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
  /**
   * MusicBrainz のリリース（MUSICBRAINZ_ALBUMID）。提案はファイルのタグ、承認画面で候補を選ぶと入る（P4-21）。
   * null / 無しなら配置でタグに触れない
   */
  release_id?: string | null
  /** 同じくリリースグループ（MUSICBRAINZ_RELEASEGROUPID） */
  release_group_id?: string | null
}

/** CD の件の照会の材料（`GET /api/inbox` の `rip`。P4-21）。`POST /api/cd/lookup` にそのまま渡す */
export type RipLookup = {
  toc: string
  /** ドライブが読んだ ISRC（音声トラック順。旧サイドカーは空） */
  isrcs: Array<string | null>
  mcn: string | null
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
  /** 追記先の album にある同名の行（P4-19。警告のみ。旧サーバでは無い） */
  same_title?: SameTitle[]
}

/** 追記先の album にある同名のトラック（P4-19） */
export type SameTitle = {
  track_id: number
  rel_path: string
  duration_ms: number | null
}

/**
 * トラック行の「Library に同名」警告（P4-19）。無ければ null。
 * 長さを添えるのは、同じタイトルの別テイク（Cover / Live）と本当の二重取り込みを人が見分けるため
 */
export function sameTitleLabel(f: Pick<InboxFile, 'same_title'>): string | null {
  const rows = f.same_title ?? []
  if (rows.length === 0) return null
  const parts = rows.map((r) => {
    const name = r.rel_path.split('/').pop() ?? r.rel_path
    return r.duration_ms == null ? name : `${name}（${formatDuration(r.duration_ms)}）`
  })
  return `Library に同名: ${parts.join('、')}`
}

/** 件の中で同名の警告が付いたトラックの数（見出しに出す） */
export function sameTitleCount(item: Pick<InboxItem, 'tracks'>): number {
  return (item.tracks ?? []).filter((f) => (f.same_title ?? []).length > 0).length
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
/** 周期監視の状態（`GET /api/inbox` の `watch`。P4-18） */
export type InboxWatch = {
  /** 最後に Inbox を確認した時刻。監視が無い / まだなら null */
  checked_at: number | null
  poll_interval_secs: number
}

/** ツールバーの「最後に確認: HH:MM:SS（N 秒ごと）」。監視が無ければ空 */
export function watchLabel(w: InboxWatch | null, formatTime: (epoch: number) => string): string {
  if (!w || w.poll_interval_secs <= 0) return ''
  const every = `${w.poll_interval_secs} 秒ごと`
  return w.checked_at == null ? `確認: ${every}（まだ）` : `最後に確認: ${formatTime(w.checked_at)}（${every}）`
}

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
  /** CD の件の照会の材料（P4-21）。CD でない件は null（旧サーバでは無い） */
  rip?: RipLookup | null
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

/** ファイルの ARTIST の全値（値そのまま。出現順。サーバの artist_values と同じ） */
export function artistValues(file: Pick<InboxFile, 'tags'>): string[] {
  return file.tags.filter(([k]) => k === 'ARTIST').map(([, v]) => v)
}

/**
 * トラックの keep_artists の初期値。ファイルが多値でなければ常に false（保存値が true でも戻す）。
 * 多値なら、保存値が無ければ true（提案）、保存値が boolean ならそれ、旧下書き（null）はサーバの
 * 旧規則と同じ「先頭値が下書きの実効アーティスト（空ならアルバムアーティスト）のまま」
 */
export function keepArtistsFor(
  file: Pick<InboxFile, 'tags'> | undefined,
  saved: DraftTrack | null,
  albumartist: string,
): boolean {
  const values = file == null ? [] : artistValues(file)
  if (values.length <= 1) return false
  if (saved == null) return true
  if (typeof saved.keep_artists === 'boolean') return saved.keep_artists
  const effective = saved.artist.trim() === '' ? albumartist.trim() : saved.artist.trim()
  return effective === values[0]
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
    // album gain の初期値は追記先の現在値、無ければ提案（CD の吸い出しは on、それ以外は off。D-74）
    return {
      ...p,
      tracks: p.tracks.map((t) => ({
        ...cloneTrack(t),
        keep_artists: keepArtistsFor(fileByKey.get(pathKey(t.rel_path)), null, p.albumartist),
      })),
      album_gain: item.destination?.album_gain ?? p.album_gain,
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
      if (s == null) return { ...cloneTrack(t), keep_artists: keepArtistsFor(file, null, saved.albumartist) }
      const keep = keepArtistsFor(file, s, saved.albumartist)
      // 保存時に「そのまま保つ」だった artist は表示文字列なので、現在のファイルの値から作り直す
      // （承認後にファイルが変わっていても古い文字列を書き戻さない）
      const artist = s.keep_artists === true && file != null ? artistValues(file).join(ARTIST_JOIN) : s.artist
      return { ...cloneTrack(s), rel_path: t.rel_path, artist, keep_artists: keep }
    }),
    album_gain: saved.album_gain,
    // 旧下書き（欄が無い）は提案（ファイルのタグ）の値。サーバの merge_saved と同じ
    release_id: saved.release_id ?? p.release_id ?? null,
    release_group_id: saved.release_group_id ?? p.release_group_id ?? null,
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
  if (d.release_id != null && d.release_id.trim() !== '' && !isMbid(d.release_id.trim())) {
    out.push(`MusicBrainz のリリース ID の形が不正: ${d.release_id}`)
  }
  if (d.release_group_id != null && d.release_group_id.trim() !== '' && !isMbid(d.release_group_id.trim())) {
    out.push(`MusicBrainz のリリースグループ ID の形が不正: ${d.release_group_id}`)
  }
  return out
}

/** MusicBrainz の MBID（UUID。大文字も通す。サーバの is_mbid と同じ） */
export function isMbid(s: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(s)
}

/** 下書きにあるディスク番号（昇順。貼り付けの写し先を選ぶ） */
export function discNumbers(d: Pick<InboxDraft, 'tracks'>): number[] {
  return [...new Set(d.tracks.map((t) => t.disc_no))].sort((a, b) => a - b)
}

/**
 * トラックリスト貼り付け（`lib/tracklist.ts` の解析結果）を下書きに写す（P2-10、D-65。CD 画面から移した）。
 * 写し先は `disc` のディスクの行で、**トラック番号で対応付ける**（ファイルの並びではない）。
 * アーティストの無い行は既存の値を保つ。アーティストを貼った行は多値の「そのまま保つ」を外す
 * （貼った値で 1 値にする。保ったままだと貼った値が書かれない）。
 * 件に無い番号・同じ番号の行が複数あるもの（どれに写すか決められない）は写さず、行数の違い・
 * 未設定の行とともに警告にする。元の draft は変えない
 */
export function applyTracklist(
  draft: InboxDraft,
  disc: number,
  parsed: Array<{ no: number; title: string; artist: string | null }>,
): { draft: InboxDraft; warnings: string[] } {
  const warnings: string[] = []
  const rows = new Map<number, number[]>()
  draft.tracks.forEach((t, i) => {
    if (t.disc_no === disc) rows.set(t.track_no, [...(rows.get(t.track_no) ?? []), i])
  })
  const count = [...rows.values()].reduce((n, v) => n + v.length, 0)
  if (parsed.length !== count) warnings.push(`貼り付けの行数 ${parsed.length} がディスク ${disc} の ${count} 曲と違う`)
  const tracks = draft.tracks.map((t) => ({ ...t }))
  const unknown: number[] = []
  const ambiguous: number[] = []
  const covered = new Set<number>()
  for (const p of parsed) {
    const at = rows.get(p.no)
    if (at == null) {
      unknown.push(p.no)
      continue
    }
    if (at.length > 1) {
      if (!ambiguous.includes(p.no)) ambiguous.push(p.no)
      continue
    }
    covered.add(p.no)
    const t = tracks[at[0]!]!
    t.title = p.title
    if (p.artist != null) {
      t.artist = p.artist
      t.keep_artists = false
    }
  }
  if (unknown.length > 0) warnings.push(`ディスク ${disc} に無い番号: ${unknown.join(', ')}`)
  if (ambiguous.length > 0) warnings.push(`番号が重複する行には写さない: ${ambiguous.join(', ')}`)
  const missing = [...rows.keys()]
    .filter((n) => !covered.has(n) && !ambiguous.includes(n))
    .sort((a, b) => a - b)
  if (missing.length > 0) warnings.push(`未設定の行: ${missing.join(', ')}`)
  return { draft: { ...draft, tracks }, warnings }
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
    release_id: opt(d.release_id ?? null),
    release_group_id: opt(d.release_group_id ?? null),
  }
}

/** 吸い出しが空のタイトルに付ける名前（`Track 01`）。候補を写すときは空欄と同じに扱う */
const PLACEHOLDER_TITLE = /^Track \d{2,}$/

/**
 * MusicBrainz の候補を下書きに写す（P4-21。ユーザ判断: ID に加えて空欄の名前も）。
 * - リリース / リリースグループの ID は常に写す（盤の識別。D-72）
 * - ディスク番号は候補の medium の位置にする（2 枚組の 2 枚目を 2 枚目として置く）
 * - アルバム名・アルバムアーティスト・日付・曲名・曲のアーティストは、下書きで**空のところだけ**埋める
 *   （曲名は吸い出しが付けた `Track NN` も空とみなす）。手で入れた値は上書きしない
 * - 曲は**トラック番号**で候補の曲に対応させる（件のファイルの並びではない）。曲のアーティストが
 *   アルバムアーティストと同じなら空のまま（配置でアルバムアーティストになる）
 */
export function applyCandidate(d: InboxDraft, c: ReleaseCandidate): InboxDraft {
  const albumartist = d.albumartist.trim() === '' ? c.artist : d.albumartist
  return {
    ...d,
    release_id: c.release_id,
    release_group_id: c.release_group_id,
    albumartist,
    album: d.album.trim() === '' ? c.title : d.album,
    date: d.date == null || d.date.trim() === '' ? c.date : d.date,
    tracks: d.tracks.map((t) => {
      const ct = c.tracks[t.track_no - 1]
      const out = { ...cloneTrack(t), disc_no: c.medium_position }
      if (ct == null) return out
      if (t.title.trim() === '' || PLACEHOLDER_TITLE.test(t.title.trim())) out.title = ct.title
      if (t.artist.trim() === '' && t.keep_artists !== true && ct.artist !== albumartist) out.artist = ct.artist
      return out
    }),
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


// ---------------------------------------------------------------- 承認画面の表の列（§12.6 Inbox）

/**
 * 表の固定列が既に出しているタグ（全タグの列から外す）。TITLE / ARTIST / 番号は編集セル、ALBUM /
 * ALBUMARTIST / DATE はアルバム単位の欄の写し、PICTURE はサムネイル列
 */
const COVERED_TAGS = new Set([
  'TITLE',
  'ARTIST',
  'ALBUM',
  'ALBUMARTIST',
  'DATE',
  'TRACKNUMBER',
  'DISCNUMBER',
  'PICTURE',
])

/** 件のファイルが持つタグのうち、固定列に無いキー（全タグの列）。ABC 順 */
export function extraTagKeys(files: Pick<InboxFile, 'tags'>[]): string[] {
  const keys = new Set<string>()
  for (const f of files) {
    for (const [k] of f.tags) {
      if (!COVERED_TAGS.has(k)) keys.add(k)
    }
  }
  return [...keys].sort()
}

/** ファイルのタグの値（多値は "; " で結合。無ければ空） */
export function tagValue(file: Pick<InboxFile, 'tags'>, key: string): string {
  return file.tags
    .filter(([k]) => k === key)
    .map(([, v]) => v)
    .join(ARTIST_JOIN)
}

export type InboxColumn = {
  id: string
  label: string
  /** edit: トラック単位で直す / album: 上の欄の写し / file: ファイルから / tag: ファイルのタグ */
  group: 'edit' | 'album' | 'file' | 'tag'
  /** 列メニューで隠せるか（番号とタイトルは常に出す） */
  hideable: boolean
}

/** 表の列（左から）。サムネイル・判定は件に該当するものがあるときだけ */
export function inboxColumns(opts: { hasPicture: boolean; hasSource: boolean; tagKeys: string[] }): InboxColumn[] {
  const cols: InboxColumn[] = [
    { id: 'disc', label: 'disc', group: 'edit', hideable: false },
    { id: 'no', label: '#', group: 'edit', hideable: false },
  ]
  if (opts.hasPicture) cols.push({ id: 'thumb', label: '画像', group: 'file', hideable: true })
  cols.push(
    { id: 'title', label: 'タイトル', group: 'edit', hideable: false },
    { id: 'artist', label: 'アーティスト', group: 'edit', hideable: true },
    { id: 'album', label: 'アルバム', group: 'album', hideable: true },
    { id: 'albumartist', label: 'アルバムアーティスト', group: 'album', hideable: true },
    { id: 'date', label: '日付', group: 'album', hideable: true },
    { id: 'category', label: 'category', group: 'album', hideable: true },
    { id: 'duration', label: '長さ', group: 'file', hideable: true },
    { id: 'codec', label: 'codec', group: 'file', hideable: true },
    { id: 'file', label: 'ファイル', group: 'file', hideable: true },
  )
  if (opts.hasSource) cols.push({ id: 'verdict', label: '判定', group: 'file', hideable: true })
  for (const k of opts.tagKeys) cols.push({ id: `tag:${k}`, label: k, group: 'tag', hideable: true })
  return cols
}

/** 隠した列の保存形（localStorage）を読む。壊れていれば空 */
export function parseHiddenColumns(raw: string | null): string[] {
  if (raw == null) return []
  try {
    const v: unknown = JSON.parse(raw)
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === 'string') : []
  } catch {
    return []
  }
}

/**
 * 横スクロールしても左に残す列の幅（px。セルの枠込み）。どの行のタグを見ているか分かるよう、
 * 番号・画像・タイトルは左端に固定する（D-85 追記）
 */
const STICKY_WIDTHS: Record<string, number> = { disc: 72, no: 72, thumb: 44, title: 292 }

export type StickyCell = { left: number; width: number; last: boolean }

/** 表示する列のうち左端に固定するものの位置（左からの累積）。固定の列は表示の先頭に続いて並ぶ前提 */
export function stickyColumns(shown: Pick<InboxColumn, 'id'>[]): Map<string, StickyCell> {
  const out = new Map<string, StickyCell>()
  let left = 0
  for (const c of shown) {
    const width = STICKY_WIDTHS[c.id]
    if (width == null) break
    out.set(c.id, { left, width, last: false })
    left += width
  }
  const ids = [...out.keys()]
  const tail = ids.length > 0 ? out.get(ids[ids.length - 1]) : undefined
  if (tail != null) tail.last = true
  return out
}
