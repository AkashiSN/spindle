import { parsePictureValue } from './artwork'
import type { ReleaseCandidate } from './cd'
import { formatDuration } from './format'
import { splitValues } from './properties'

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
  /**
   * ファイルのタグの変更（D-86）。キー（大文字）→ 値の配列、null はそのタグを消す。書くのはここにある
   * キーだけ。上の欄が扱うキー（COVERED_TAG_KEYS）と同一性のキー（isLockedTagKey）は不可
   */
  tags?: Record<string, string[] | null>
  /** 埋め込み画像を差し替える（`<mime>:<sha256hex>`。アップロード済みの画像。D-86）。無ければファイルの画像 */
  picture?: string | null
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
 * 長さを添えるのは、同じタイトルの別テイク（Cover / Live）と本当の二重取り込みを人が見分けるため。
 * 件の曲の長さも並べる（P4-22。Library 側だけでは比べられない）
 */
export function sameTitleLabel(f: Pick<InboxFile, 'same_title' | 'duration_ms'>): string | null {
  const rows = f.same_title ?? []
  if (rows.length === 0) return null
  const parts = rows.map((r) => {
    const name = r.rel_path.split('/').pop() ?? r.rel_path
    return r.duration_ms == null ? name : `${name}（${formatDuration(r.duration_ms)}）`
  })
  const own = f.duration_ms == null ? '' : `／この曲 ${formatDuration(f.duration_ms)}`
  return `Library に同名: ${parts.join('、')}${own}`
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
  /** 再生リストの購読から落とした曲の購読 id と再生リスト上の位置（D-78）。それ以外は無い */
  subscription_id?: number
  position?: number
}

/** 追記先の既存 album（GET /api/inbox の destination。D-70） */
export type InboxDestination = {
  album_id: number
  album: string | null
  track_count: number
  max_track_no: number
  /** 追記先の album gain の属性（チェックボックスの初期値。D-74） */
  album_gain: boolean
  /** 追記先の既存の番号 `[disc, track]`（昇順。P4-22。旧サーバでは無い） */
  numbers?: Array<[number, number]>
  /** 追記先の曲がディスク番号を持つか（false なら配置で `DISCNUMBER` を書かない。旧サーバでは無い） */
  uses_disc?: boolean
}

/** 件が参照する購読の直近の同期（`GET /api/inbox` の `subscriptions`。P4-22） */
export type InboxSubscriptionSync = {
  id: number
  album: string
  synced_at: number | null
  /** 番号を揃えた行数・名前を変えた行数 */
  moved: number
  renamed: number
  tags_batch_id: number | null
  rename_batch_id: number | null
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
  /** 破棄待ち（却下した件の「削除」の時刻。GC が retention 日後にファイルと件を消す。D-90）。旧サーバでは無い */
  discard_requested_at?: number | null
  /** CD の取り込みの表の画像（Cover Art Archive から自動で取ったもの。`<mime>:<sha256hex>`。D-91）。旧サーバでは無い */
  caa_picture?: string | null
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

/** 破棄待ちの件を GC が消す時刻（D-90）。破棄待ちでない・日数が分からなければ null */
export function discardDeadline(
  item: Pick<InboxItem, 'state' | 'discard_requested_at'>,
  retentionDays: number | null,
): number | null {
  if (item.state !== 'rejected' || item.discard_requested_at == null || retentionDays == null) return null
  return item.discard_requested_at + retentionDays * 86_400
}

/** 破棄待ちの表示（「YYYY-MM-DD HH:MM 以降の GC でファイルを消す」）。破棄待ちでなければ null */
export function discardLabel(
  item: Pick<InboxItem, 'state' | 'discard_requested_at'>,
  retentionDays: number | null,
  formatTime: (epoch: number) => string,
): string | null {
  if (item.state !== 'rejected' || item.discard_requested_at == null) return null
  const at = discardDeadline(item, retentionDays)
  return at == null
    ? '削除待ち（GC がファイルを消す。それまでは取り消せる）'
    : `削除待ち: ${formatTime(at)} 以降の GC でファイルを消す（それまでは取り消せる）`
}

/**
 * 件の出どころ（見出しのバッジ）。未配置はサイドカー（`rip` / 曲ごとの出どころ）で決める。
 * **配置済みはサイドカーが配置の後に Inbox から消えている**ので、取り込み元のディレクトリだけで
 * 決める（CD の吸い出しは `CD/`、YouTube の取得は `youtube/` の下に公開される）。消し損ねや
 * 差し替えで残ったサイドカーは配置したものと限らないので見ない。
 * 表示だけに使う。CD 用の処理（照会・照合）はパスで判定しない
 */
export function itemSource(item: Pick<InboxItem, 'state' | 'rel_dir' | 'rip' | 'tracks'>): 'CD' | 'YouTube' | '手置き' {
  if (item.state === 'placed') {
    const top = item.rel_dir.split('/')[0].toLowerCase()
    return top === 'cd' ? 'CD' : top === 'youtube' ? 'YouTube' : '手置き'
  }
  if (item.rip != null) return 'CD'
  if (item.tracks.some((f) => f.source != null)) return 'YouTube'
  return '手置き'
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
    // 変更が無ければ欄ごと持たない（旧い下書き・提案と同じ形）
    tags: t.tags != null && Object.keys(t.tags).length > 0 ? { ...t.tags } : undefined,
    picture: t.picture ?? undefined,
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

/**
 * 取り込みの埋め込み画像の URL。承認前は Inbox のファイルから読む（`/api/inbox/:id/artwork/:hash`）。
 * **配置済み（placed）はファイルが Library に移っていて Inbox からは 404** なので、配置で登録された
 * Library の画像置き場（`/api/artwork/:hash`）から出す。`size` は Library 側のサムネイルの大きさ
 */
export function artworkUrl(item: Pick<InboxItem, 'id' | 'state'>, hash: string, size?: 256 | 768): string {
  if (item.state === 'placed') return size == null ? `/api/artwork/${hash}` : `/api/artwork/${hash}?size=${size}`
  return `/api/inbox/${item.id}/artwork/${hash}`
}

/**
 * 一覧のサムネイルの URL。ファイルの埋め込み画像があればそれ（`itemCover`）、無ければ下書き（保存済み、無ければ
 * 提案）で差し替える画像。提案には CD の取り込みの表の画像（Cover Art Archive から自動で取ったもの。D-91）が
 * 入るので、吸い出したばかりの画像の無い盤でもジャケットが出る。置いた画像は `/api/artwork/:hash`（256px）
 */
export function itemThumbUrl(item: Pick<InboxItem, 'id' | 'state' | 'tracks' | 'draft' | 'proposal'>): string | null {
  const embedded = itemCover(item)
  if (embedded != null) return artworkUrl(item, embedded, 256)
  const tracks = (item.draft ?? item.proposal).tracks
  for (const t of tracks) {
    const p = t.picture == null ? null : parsePictureValue(t.picture)
    if (p != null) return `/api/artwork/${p.hash}?size=256`
  }
  return null
}

/**
 * 開いている取り込みの下書きを、一覧の取り直しで作り直すか（D-91）。`base` は前回この画面が初期値にした下書き、
 * `fresh` は今の一覧から作った初期値（`draftFrom`）。**人がまだ触っていない**（`draft` が `base` のまま）ときだけ
 * 新しい初期値を返す。走査の後に提案だけが変わった（表の画像が取れた等）ときに画面へ出すため。触っていれば
 * null（編集中の値を上書きしない）
 */
export function refreshedDraft(base: InboxDraft, draft: InboxDraft, fresh: InboxDraft): InboxDraft | null {
  const same = (a: InboxDraft, b: InboxDraft) => JSON.stringify(a) === JSON.stringify(b)
  if (same(base, fresh) || !same(draft, base)) return null
  return fresh
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
    if (!known.has(key)) out.push(`取り込みに無いファイル: ${t.rel_path}`)
    if (seen.has(key)) out.push(`下書きに同じファイルが 2 回: ${t.rel_path}`)
    seen.add(key)
    for (const key of Object.keys(t.tags ?? {})) {
      if (!isValidTagKey(key)) out.push(`タグのキーが不正: ${key}（${t.rel_path}。空・小文字・= や制御文字は使えない）`)
      else if (COVERED_TAG_KEYS.includes(key)) out.push(`${key} は上の欄で直す（${t.rel_path}）`)
      else if (isLockedTagKey(key)) out.push(`${key} は曲・盤の識別に使うので直せない（${t.rel_path}）`)
    }
    if (t.picture != null) {
      const p = parsePictureValue(t.picture)
      if (p == null || !DRAFT_PICTURE_MIMES.includes(p.mime)) out.push(`画像の指定が不正: ${t.picture}（${t.rel_path}）`)
    }
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
    tracks: d.tracks.map((t) => {
      const out: DraftTrack = {
        rel_path: t.rel_path,
        disc_no: t.disc_no,
        track_no: t.track_no,
        title: t.title.trim(),
        artist: t.artist.trim(),
        keep_artists: t.keep_artists === true,
      }
      const tags = Object.entries(t.tags ?? {})
      if (tags.length > 0) {
        out.tags = Object.fromEntries(
          tags.map(([k, vs]) => {
            const kept = vs?.map((v) => v.trim()).filter((v) => v !== '') ?? []
            return [k, kept.length > 0 ? kept : null]
          }),
        )
      }
      if (t.picture != null) out.picture = t.picture
      return out
    }),
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

/** 番号の表示（2 枚目以降だけ disc を前置。`165`、`2-03`） */
function numberText(disc: number, track: number): string {
  return disc > 1 ? `${disc}-${String(track).padStart(2, '0')}` : String(track)
}

/** 番号の並びを短く（連続は範囲に: `165`、`165〜167`、`12, 165`）。disc ごと */
function numbersText(nums: Array<[number, number]>): string {
  const sorted = [...nums].sort((a, b) => a[0] - b[0] || a[1] - b[1])
  const parts: string[] = []
  let i = 0
  while (i < sorted.length) {
    let j = i
    while (j + 1 < sorted.length && sorted[j + 1][0] === sorted[i][0] && sorted[j + 1][1] === sorted[j][1] + 1) j++
    const [d, a] = sorted[i]
    parts.push(j === i ? numberText(d, a) : `${numberText(d, a)}〜${numberText(d, sorted[j][1])}`)
    i = j + 1
  }
  return parts.join(', ')
}

/** 購読から落とした曲（サイドカーに購読 id と位置がある）の下書きの番号 */
export function subscriptionNumbers(
  item: Pick<InboxItem, 'tracks'>,
  draft: Pick<InboxDraft, 'tracks'>,
): Array<[number, number]> {
  const subscribed = new Set(
    item.tracks.filter((f) => f.source?.subscription_id != null).map((f) => f.rel_path),
  )
  return draft.tracks.filter((t) => subscribed.has(t.rel_path)).map((t) => [t.disc_no, t.track_no])
}

/**
 * 宛先の表示（P4-22）。購読から落とした曲を含む件は、番号が再生リストの位置（同期が空けた番号）に入るので
 * 「番号 165 に入る」と出す（「max + 1 から」は購読の件には当たらない）。そうでなければ従来の文言。
 * `overlap` は下書きの番号が宛先の既存の番号と重なるときの警告（承認はサーバが 400 で止める）
 */
/**
 * 配置で `DISCNUMBER` を書かないか（サーバの `omits_disc` と同じ規則）。追記先の曲がどれもディスク番号を
 * 持たず、下書きの disc_no の最大が 1 で（人が 2 以上にした値は書く）、CD の取り込み（`rip` あり）でないとき。ディスク番号の無い album に 1 を書くと、並べ替えで
 * 追記した曲だけが末尾に回る
 */
export function omitsDisc(
  item: Pick<InboxItem, 'destination' | 'rip'>,
  draft: Pick<InboxDraft, 'tracks'>,
): boolean {
  const d = item.destination
  if (d == null || d.uses_disc !== false || d.track_count === 0 || item.rip != null) return false
  return draft.tracks.every((t) => t.disc_no <= 1)
}

export function destinationText(
  item: Pick<InboxItem, 'destination' | 'tracks' | 'rip'>,
  draft: Pick<InboxDraft, 'tracks'>,
): { label: string; overlap: string | null } | null {
  const d = item.destination
  if (d == null) return null
  const taken = new Set((d.numbers ?? []).map(([disc, no]) => `${disc}:${no}`))
  const clash: Array<[number, number]> = draft.tracks
    .filter((t) => taken.has(`${t.disc_no}:${t.track_no}`))
    .map((t) => [t.disc_no, t.track_no])
  const overlap = clash.length === 0 ? null : `番号 ${numbersText(clash)} は宛先に既にある（③ で直す）`
  const subscribed = subscriptionNumbers(item, draft)
  const disc = omitsDisc(item, draft) ? '。ディスク番号は付けない（宛先の曲に無い）' : ''
  if (subscribed.length === 0) return { label: (destinationLabel(d) ?? '') + disc, overlap }
  const name = d.album == null ? '既存のアルバム' : `既存の『${d.album}』`
  return {
    label: `宛先: ${name}（${d.track_count} 曲）に追加。番号 ${numbersText(subscribed)} に入る（再生リストの位置。空けてある番号）${disc}`,
    overlap,
  }
}

/**
 * 購読の同期が番号を空けた経緯（P4-22）。購読から落とした曲が無ければ null。要約は購読の**直近の**同期なので、
 * 件を落とした後にもう一度同期していれば揃え直しは 0 件になる（そのときは「承認しても既存の曲は動かない」）
 */
export function syncNote(
  item: Pick<InboxItem, 'tracks'>,
  draft: Pick<InboxDraft, 'tracks'>,
  subs: InboxSubscriptionSync[],
  formatTime: (epoch: number) => string,
): string | null {
  const ids = [...new Set(item.tracks.map((f) => f.source?.subscription_id).filter((x): x is number => x != null))]
  if (ids.length === 0) return null
  const nums = numbersText(subscriptionNumbers(item, draft))
  const notes = ids.map((id) => {
    const s = subs.find((x) => x.id === id)
    if (s == null) return `購読 #${id} の同期の記録が無い`
    const when = s.synced_at == null ? '' : `（${formatTime(s.synced_at)}）`
    if (s.moved === 0) {
      return `直近の同期${when}では番号の揃え直しは無かった。承認しても既存の曲の番号は動かない`
    }
    const batches = [s.tags_batch_id, s.rename_batch_id].filter((b): b is number => b != null).map((b) => `#${b}`)
    const renamed = s.renamed > 0 ? `、${s.renamed} 曲のファイル名を直して` : ''
    return (
      `同期${when}で既存の ${s.moved} 曲の番号を揃え${renamed} ${nums} を空けた` +
      `${batches.length > 0 ? `（バッチ ${batches.join(' / ')}。履歴で巻き戻せる）` : ''}。承認しても既存の曲は動かない`
    )
  })
  return notes.join(' / ')
}

/** 判定バッジの文言（D-70） */
export function verdictLabel(s: InboxSource): { text: string; ok: boolean } {
  return s.verdict === 'ok' ? { text: '判定済み', ok: true } : { text: `未判定（${s.verdict}）`, ok: false }
}


// ---------------------------------------------------------------- タグの変更（D-86）

/** 上の欄が扱うキー。`DraftTrack.tags` では受け付けない（サーバの COVERED_TAG_KEYS と同じ） */
export const COVERED_TAG_KEYS: readonly string[] = [
  'TITLE',
  'ARTIST',
  'ALBUM',
  'ALBUMARTIST',
  'DATE',
  'TRACKNUMBER',
  'DISCNUMBER',
  'DISCTOTAL',
  'PICTURE',
]

/** 曲・盤の同一性の判定に使うキー（鍵をかける。サーバの is_locked_tag_key と同じ。D-86） */
export function isLockedTagKey(key: string): boolean {
  return key === 'SOURCE_URL' || key.startsWith('MUSICBRAINZ_')
}

/** タグのキーの形（大文字・空でない・前後に空白が無い・= と制御文字を含まない。サーバの is_valid_tag_key） */
export function isValidTagKey(key: string): boolean {
  return key !== '' && key === key.trim() && key === key.toUpperCase() && /^[ -<>-}]+$/.test(key)
}

/** 差し替えに使える画像の形式（サーバの DRAFT_PICTURE_MIMES） */
const DRAFT_PICTURE_MIMES: readonly string[] = ['image/jpeg', 'image/png', 'image/webp']

/** ファイルのタグの値（多値は "; " で結合。無ければ空） */
export function tagValue(file: Pick<InboxFile, 'tags'>, key: string): string {
  return file.tags
    .filter(([k]) => k === key)
    .map(([, v]) => v)
    .join(ARTIST_JOIN)
}

/** 下書きを当てた後のタグの値（表示用。変更が無ければファイルの値） */
export function effectiveTag(file: Pick<InboxFile, 'tags'> | undefined, t: Pick<DraftTrack, 'tags'>, key: string): string {
  const over = t.tags?.[key]
  if (over !== undefined) return over == null ? '' : over.join(ARTIST_JOIN)
  return file == null ? '' : tagValue(file, key)
}

/**
 * タグのセルを直す。入力は "; " 区切りの多値。ファイルの値と同じに戻したら変更を消し、空にしたら
 * ファイルにあるキーは null（消す）、無いキーは変更を消す
 */
export function setTrackTag(t: DraftTrack, file: Pick<InboxFile, 'tags'> | undefined, key: string, text: string): DraftTrack {
  const values = splitValues(text)
  const original = file == null ? [] : splitValues(tagValue(file, key))
  const tags = { ...(t.tags ?? {}) }
  const same = values.length === original.length && values.every((v, i) => v === original[i])
  if (same) delete tags[key]
  else if (values.length === 0) tags[key] = null
  else tags[key] = values
  return { ...t, tags }
}

/** タグが変わっているか（セルの印） */
export function tagChanged(file: Pick<InboxFile, 'tags'> | undefined, t: Pick<DraftTrack, 'tags'>, key: string): boolean {
  return t.tags?.[key] !== undefined && effectiveTag(file, t, key) !== (file == null ? '' : tagValue(file, key))
}

/** 「タグを追加」のキーの検証。問題が無ければ null */
export function newTagKeyProblem(raw: string, existing: readonly string[]): string | null {
  const key = raw.trim().toUpperCase()
  if (!isValidTagKey(key)) return 'キーが空か、使えない文字（= や制御文字）を含んでいる'
  if (COVERED_TAG_KEYS.includes(key)) return `${key} は左の列（または ②）で直す`
  if (isLockedTagKey(key)) return `${key} は曲・盤の識別に使うので追加できない`
  if (existing.includes(key)) return `${key} の列は既にある`
  return null
}

// ---------------------------------------------------------------- 画像の差し替え（D-86）

/** トラックの画像の同一性の鍵（sha256）。下書きで差し替えていればその画像、無ければファイルの代表 */
export function trackPictureHash(file: Pick<InboxFile, 'tags'> | undefined, t: Pick<DraftTrack, 'picture'>): string | null {
  if (t.picture != null) return parsePictureValue(t.picture)?.hash ?? null
  return file == null ? null : pictureOf(file)
}

/** トラックの画像の URL。差し替えた画像は `/api/artwork/:hash`、ファイルの画像は取り込みの埋め込み画像（配置済みは Library の画像置き場） */
export function trackPictureUrl(item: Pick<InboxItem, 'id' | 'state'>, file: Pick<InboxFile, 'tags'> | undefined, t: Pick<DraftTrack, 'picture'>): string | null {
  if (t.picture != null) {
    const p = parsePictureValue(t.picture)
    return p == null ? null : `/api/artwork/${p.hash}`
  }
  const h = file == null ? null : pictureOf(file)
  return h == null ? null : artworkUrl(item, h)
}

export type PictureState = {
  /** none: どの曲にも無い / uniform: 全曲同じ / mixed: 曲ごとに違う・一部に無い */
  mode: 'none' | 'uniform' | 'mixed'
  kinds: number
  missing: number
  /** 差し替えた曲の数 */
  changed: number
}

export function pictureState(files: ReadonlyMap<string, Pick<InboxFile, 'tags'>>, d: Pick<InboxDraft, 'tracks'>): PictureState {
  const hashes = d.tracks.map((t) => trackPictureHash(files.get(t.rel_path), t))
  const kinds = new Set(hashes.filter((h): h is string => h != null)).size
  const missing = hashes.filter((h) => h == null).length
  const changed = d.tracks.filter((t) => t.picture != null).length
  const mode = kinds === 0 ? 'none' : kinds === 1 && missing === 0 ? 'uniform' : 'mixed'
  return { mode, kinds, missing, changed }
}

/** 画像の当て先: 全曲 / 画像の無い曲 / 1 曲（下書きの位置） */
export type PictureTarget = 'all' | 'missing' | number

/** 下書きに画像を当てる（`value` は `<mime>:<sha256hex>`） */
export function applyPicture(
  d: InboxDraft,
  files: ReadonlyMap<string, Pick<InboxFile, 'tags'>>,
  value: string,
  target: PictureTarget,
): InboxDraft {
  return {
    ...d,
    tracks: d.tracks.map((t, i) => {
      const hit =
        target === 'all' || (target === 'missing' && trackPictureHash(files.get(t.rel_path), t) == null) || target === i
      return hit ? { ...t, picture: value } : t
    }),
  }
}

/** 画像の差し替えを全部戻す */
export function resetPictures(d: InboxDraft): InboxDraft {
  return { ...d, tracks: d.tracks.map((t) => ({ ...t, picture: null })) }
}

/**
 * 下書きの画像が、Cover Art Archive から自動で取った表の画像（`item.caa_picture`）そのものか（D-91）。
 * 全曲がその 1 枚のときだけ true（人が差し替えた・外した曲があれば false）
 */
export function usesCaaPicture(item: Pick<InboxItem, 'caa_picture'>, d: Pick<InboxDraft, 'tracks'>): boolean {
  const caa = item.caa_picture
  return caa != null && d.tracks.length > 0 && d.tracks.every((t) => t.picture === caa)
}

// ---------------------------------------------------------------- 承認画面の表の列（§12.6 Inbox）

/** 固定列が出しているタグ（全タグの列から外す） */
const COLUMN_TAGS = new Set(['TITLE', 'ARTIST', 'ALBUM', 'ALBUMARTIST', 'DATE', 'TRACKNUMBER', 'DISCNUMBER', 'PICTURE'])

/** 件のファイルが持つタグと、下書きで足したキーのうち、固定列に無いもの（全タグの列）。ABC 順 */
export function extraTagKeys(files: Pick<InboxFile, 'tags'>[], tracks: Pick<DraftTrack, 'tags'>[] = []): string[] {
  const keys = new Set<string>()
  for (const f of files) {
    for (const [k] of f.tags) {
      if (!COLUMN_TAGS.has(k)) keys.add(k)
    }
  }
  for (const t of tracks) {
    for (const [k, v] of Object.entries(t.tags ?? {})) {
      if (v != null && !COLUMN_TAGS.has(k)) keys.add(k)
    }
  }
  return [...keys].sort()
}

export type InboxColumn = {
  id: string
  label: string
  /** edit: トラック単位で直す / album: アルバム単位（どの行で直しても全行）/ file: ファイルから / tag: ファイルのタグ */
  group: 'edit' | 'album' | 'file' | 'tag'
  /** 列メニューで隠せるか（番号とタイトルは常に出す） */
  hideable: boolean
  /** ダブルクリックで直せるか */
  editable: boolean
  /** 同一性に使うタグ（鍵。D-86） */
  locked?: boolean
}

/** 表の列（左から）。サムネイル・判定は件に該当するものがあるときだけ */
export function inboxColumns(opts: { hasPicture: boolean; hasSource: boolean; tagKeys: string[] }): InboxColumn[] {
  const cols: InboxColumn[] = [
    { id: 'disc', label: 'disc', group: 'edit', hideable: false, editable: true },
    { id: 'no', label: '#', group: 'edit', hideable: false, editable: true },
  ]
  if (opts.hasPicture) cols.push({ id: 'thumb', label: '画像', group: 'edit', hideable: true, editable: true })
  cols.push(
    { id: 'title', label: 'タイトル', group: 'edit', hideable: false, editable: true },
    { id: 'artist', label: 'アーティスト', group: 'edit', hideable: true, editable: true },
    { id: 'album', label: 'アルバム', group: 'album', hideable: true, editable: true },
    { id: 'albumartist', label: 'アルバムアーティスト', group: 'album', hideable: true, editable: true },
    { id: 'date', label: '日付', group: 'album', hideable: true, editable: true },
    { id: 'category', label: 'category', group: 'album', hideable: true, editable: false },
    { id: 'duration', label: '長さ', group: 'file', hideable: true, editable: false },
    { id: 'codec', label: 'codec', group: 'file', hideable: true, editable: false },
    { id: 'file', label: 'ファイル', group: 'file', hideable: true, editable: false },
  )
  if (opts.hasSource) cols.push({ id: 'verdict', label: '判定', group: 'file', hideable: true, editable: false })
  for (const k of opts.tagKeys) {
    const locked = isLockedTagKey(k)
    cols.push({
      id: `tag:${k}`,
      label: k,
      group: 'tag',
      hideable: true,
      editable: !locked && !COVERED_TAG_KEYS.includes(k),
      locked,
    })
  }
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

/**
 * 下書きの変更の数（見出しの「変更 N 件」）。アルバム単位の欄（提案と違うもの。album gain の基準は
 * 追記先の現在値）、トラックの番号・タイトル・アーティスト、タグの変更キー、画像の差し替えを数える。
 * 配置済みの件には使わない（サイドカーが Inbox から消えた後の提案と比べることになり、CD 由来の初期値が差に見える）
 */
export function draftChangeCount(item: Pick<InboxItem, 'proposal' | 'destination'>, d: InboxDraft): number {
  const p = item.proposal
  let n = 0
  if (d.albumartist !== p.albumartist) n++
  if (d.album !== p.album) n++
  if ((d.date ?? '') !== (p.date ?? '')) n++
  if ((d.category ?? null) !== (p.category ?? null)) n++
  if (d.album_gain !== (item.destination?.album_gain ?? p.album_gain)) n++
  const byPath = new Map(p.tracks.map((t) => [t.rel_path, t]))
  for (const t of d.tracks) {
    const o = byPath.get(t.rel_path)
    if (o != null) {
      if (t.disc_no !== o.disc_no) n++
      if (t.track_no !== o.track_no) n++
      if (t.title !== o.title) n++
      if (t.artist !== o.artist && t.keep_artists !== true) n++
    }
    n += Object.keys(t.tags ?? {}).length
    // 画像は提案（CD の表の画像。D-91）と違うときだけ。提案に無いトラックは指定があれば変更
    if ((t.picture ?? null) !== (o?.picture ?? null)) n++
  }
  return n
}
