// アップロード画面の純粋ロジック（設計 docs/development/2026-11-25-upload-design.md §4.2・§5.2）。
// 送信の区切り、③ の状態の文言と色、④ の押せる条件、③ の選択の PATCH 本文、前回の続きの帯

import type {
  UploadChoices,
  UploadGroupView,
  UploadMemberView,
  UploadOriginView,
  UploadPatch,
  UploadStatus,
  UploadView,
} from '../api/types'
import { operationErrorMessage } from './operations'

/** 1 回の PUT で送る大きさ（Cloudflare Tunnel の 100MB 上限の内側。サーバは 40 MiB まで受ける） */
export const CHUNK_BYTES = 32 * 1024 * 1024
/** 「ファイルを選ぶ」で受ける拡張子（音声の許可リストと zip） */
export const ACCEPT_EXTS = ['flac', 'wav', 'm4a', 'aif', 'aiff', 'mp3', 'opus', 'zip']
/** 同時に送るファイル数 */
export const PARALLEL_SENDS = 2

export type Tone = 'ok' | 'info' | 'warn' | 'ng'

/** `[start, end)` の区間に分ける。0 バイトは空の区間 1 つ（空の PUT で完了させる） */
export function chunkRanges(size: number, chunk = CHUNK_BYTES): Array<[number, number]> {
  if (size <= 0) return [[0, 0]]
  const out: Array<[number, number]> = []
  for (let s = 0; s < size; s += chunk) out.push([s, Math.min(s + chunk, size)])
  return out
}

/** サーバに渡す相対パス: ドロップで辿ったパス → フォルダ選択の相対パス → 名前 */
export function clientPath(file: File, dropPath?: string): string {
  if (dropPath != null && dropPath !== '') return dropPath
  if (file.webkitRelativePath !== '') return file.webkitRelativePath
  return file.name
}

/** 送るか。隠しファイル（`.DS_Store`、AppleDouble の `._*`）は送らない */
export function shouldSend(path: string): boolean {
  const base = path.split('/').pop() ?? path
  return !base.startsWith('.')
}

export function statusLabel(s: UploadStatus): string {
  switch (s.kind) {
    case 'library_duplicate':
      return '同じ音声がライブラリにある'
    case 'inbox_exists':
      return 'Inbox に同じ件がある'
    case 'append_to':
      return `ライブラリの『${s.album}』（${s.track_count} 曲）に追加`
    case 'needs_name':
      return 'Inbox で名前を入れる'
    case 'new':
      return '新規'
  }
}

export function statusTone(s: UploadStatus): Tone {
  switch (s.kind) {
    case 'library_duplicate':
      return 'ng'
    case 'inbox_exists':
    case 'needs_name':
      return 'warn'
    case 'append_to':
      return 'info'
    case 'new':
      return 'ok'
  }
}

function parentOf(display: string): string {
  const i = display.lastIndexOf('/')
  return i < 0 ? display : display.slice(0, i)
}

function nameLabel(albumartist: string, album: string | null, hint: string): string {
  if (album == null) return `アルバム名なし（${hint}）`
  return `${albumartist === '' ? '(アーティストなし)' : albumartist} — ${album}`
}

/** 件の名前。アルバム名が無ければ、どこから来たか（最初の曲の親のパス）を添える */
export function groupLabel(g: UploadGroupView): string {
  const first = g.tracks[0]?.display ?? ''
  return nameLabel(g.albumartist, g.album, parentOf(first))
}

/** 置く件と曲の数（凍結した後は計画の件） */
function planCounts(v: UploadView): { groups: number; tracks: number } {
  if (v.publish != null) {
    return { groups: v.publish.groups.length, tracks: v.publish.groups.reduce((n, g) => n + g.track_count, 0) }
  }
  const inc = v.groups.filter((g) => g.included)
  return { groups: inc.length, tracks: inc.reduce((n, g) => n + g.track_count, 0) }
}

export function publishLabel(v: UploadView): string {
  const c = planCounts(v)
  return `${c.groups} 件 ${c.tracks} 曲を Inbox へ置く`
}

const live = (o: UploadOriginView) => !o.removed && !o.damaged

function zipPending(o: UploadOriginView): boolean {
  return o.class === 'zip' && o.complete && o.zip?.error == null && (o.zip == null || o.zip.unpacking || o.zip.generation === 0)
}

/**
 * 「Inbox へ置く」を押せるか（サーバの凍結の条件と同じ順。§4.2 ④）。凍結した後は、止まった失敗が
 * あってジョブが動いていないときだけ押し直せる（理由は出さない。④ が凍結後の表示に変わる）
 */
export function canPublish(v: UploadView, sending: boolean): { ok: boolean; reason: string | null } {
  if (v.publish_requested_at != null || v.publish != null) {
    return { ok: v.publish_error != null && !v.busy, reason: null }
  }
  if (sending) return { ok: false, reason: '送信中' }
  const origins = v.origins.filter(live)
  if (v.busy || origins.some(zipPending)) return { ok: false, reason: 'zip を展開中' }
  if (origins.some((o) => !o.complete)) return { ok: false, reason: '送信が途中で止まったファイルがある' }
  if (origins.some((o) => o.class === 'zip' && o.zip?.error != null)) return { ok: false, reason: '展開できなかった zip がある' }
  const inc = v.groups.filter((g) => g.included)
  if (inc.length === 0) return { ok: false, reason: '置く件が無い' }
  if (inc.some((g) => g.track_count === 0)) return { ok: false, reason: '曲の無い件がある' }
  return { ok: true, reason: null }
}

/** 「送ったものを捨てる」を押せるか。公開が止まった失敗のものは、ジョブが動いていなければ捨てられる */
export function canDiscard(v: UploadView): boolean {
  if (v.busy) return false
  return v.publish_requested_at == null || v.publish_error != null
}

/** 選んだ件を 1 件にまとめた後の `merges`（表の順。先頭の件の名前になる）。2 件未満なら今のまま */
export function mergeSelected(v: UploadView, keys: string[]): string[][] {
  const picked = v.groups.map((g) => g.key).filter((k) => keys.includes(k))
  if (picked.length < 2) return v.choices.merges
  return [...v.choices.merges, picked]
}

/**
 * 外す / 入れ直すの PATCH 本文。既定（同じ音声がライブラリにある曲・件は外れる）と違うときだけ
 * 記録する。外すは常に excluded に入れる（サーバは excluded を優先する）
 */
function togglePatch(exc: string[], inc: string[], key: string, nowIncluded: boolean, defaultIncluded: boolean) {
  const e = exc.filter((k) => k !== key)
  const i = inc.filter((k) => k !== key)
  if (nowIncluded) e.push(key)
  else if (!defaultIncluded) i.push(key)
  return { e, i }
}

export function fileTogglePatch(c: UploadChoices, m: UploadMemberView, isTrack: boolean): UploadPatch {
  const def = isTrack ? m.library_match == null : true
  const { e, i } = togglePatch(c.excluded_files, c.included_files, m.staged, m.included, def)
  return { excluded_files: e, included_files: i }
}

export function groupTogglePatch(c: UploadChoices, g: UploadGroupView): UploadPatch {
  const def = !g.statuses.some((s) => s.kind === 'library_duplicate')
  const { e, i } = togglePatch(c.excluded_groups, c.included_groups, g.key, g.included, def)
  return { excluded_groups: e, included_groups: i }
}

export type OriginAction = 'remove' | 'unpack' | null

/** ② の行の状態（受け取り終えた後。送信中の進み具合は画面側の送信キューが持つ） */
export function originState(o: UploadOriginView): { label: string; tone: Tone; action: OriginAction } {
  if (o.damaged) return { label: '壊れた。外して送り直す', tone: 'ng', action: 'remove' }
  if (!o.complete) return { label: '送信が途中で止まった', tone: 'warn', action: 'remove' }
  if (o.class !== 'zip') return { label: '送信済み', tone: 'ok', action: null }
  const z = o.zip
  if (z?.error != null) {
    return { label: `展開できなかった: ${z.error.reason}`, tone: 'ng', action: z.error.retryable ? 'unpack' : 'remove' }
  }
  if (z?.unpacking === true) return { label: '展開中', tone: 'info', action: null }
  if (z == null || z.generation === 0) return { label: '展開待ち', tone: 'info', action: null }
  return { label: '展開済み', tone: 'ok', action: null }
}

/** ⑤ に並べる、置き終えた件 */
export function publishedDirs(v: UploadView): Array<{ key: string; label: string; dir: string; tracks: number }> {
  if (v.publish == null) return []
  const out: Array<{ key: string; label: string; dir: string; tracks: number }> = []
  for (const g of v.publish.groups) {
    if (g.stage !== 'published') continue
    out.push({ key: g.key, label: nameLabel(g.albumartist, g.album, g.base_dest), dir: g.dest, tracks: g.track_count })
  }
  return out
}

/** 公開を要求して、まだ全件を置き終えていない（止まった失敗も無い） */
export function publishing(v: UploadView): boolean {
  if (v.publish_requested_at == null || v.publish_error != null) return false
  return v.publish == null || v.publish.groups.some((g) => g.stage !== 'published')
}

/** 送ったものが動いている途中か（展開・公開。画面はこの間 2 秒ごとに取り直す） */
export function inProgress(v: UploadView): boolean {
  return v.busy || publishing(v) || unpackPending(v)
}

/** 前回の続きに出すもの（新しい順。送ったものが何も無いものは出さない） */
export function resumeCandidates(items: UploadView[]): UploadView[] {
  return items.filter((v) => v.origins.some((o) => !o.removed)).sort((a, b) => b.created_at - a.created_at)
}

function shortDateTime(epoch: number): string {
  const d = new Date(epoch * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${d.getMonth() + 1}/${d.getDate()} ${p(d.getHours())}:${p(d.getMinutes())}`
}

/** 前回の続きの帯の文言（いちばん新しいもの。画面は 1 件ずつ `[v]` で呼ぶ） */
export function resumeBanner(items: UploadView[]): string | null {
  const v = resumeCandidates(items)[0]
  if (v == null) return null
  const c = planCounts(v)
  return `前回の送信で Inbox へまだ置いていないものがあります（${shortDateTime(v.created_at)}、${c.groups} 件 ${c.tracks} 曲）`
}

const KNOWN: Record<string, string> = {
  busy: '展開か公開が動いている。終わるまで待つ',
  incomplete: '送信か展開が終わっていないものがある',
  empty: '置く件が無い',
  exists: '同じ名前のファイルはもう送ってある',
  no_space: 'Inbox の空き容量が足りない',
  not_retryable: 'この zip は展開し直せない',
  not_found: 'アップロードが見つからない（捨てたか、期限が切れた）',
  origin_not_found: '送ったファイルが見つからない',
  inbox_unavailable: 'Inbox が設定されていない',
}

/** アップロードの API の失敗を文言に（413 の本文は JSON でないことがある） */
export function uploadErrorMessage(status: number, body: unknown): string {
  const b = (body ?? {}) as { error?: string; message?: string }
  if (b.error != null && KNOWN[b.error] != null) return KNOWN[b.error]!
  if (b.error === 'limit') return `上限を超える: ${b.message ?? ''}`
  if (status === 413) return '1 回に送れる大きさを超えた（HTTP 413）'
  return operationErrorMessage(status, body)
}

/** ② の zip に展開待ち・展開中のものがある */
export function unpackPending(v: UploadView): boolean {
  return v.origins.filter(live).some(zipPending)
}

/** 前の仕事が終わってから次を走らせる（失敗しても次へ進む）。③ の PATCH は項目を置き換えるので、
 *  本文は前の応答を反映した後に作る必要がある */
export function serialQueue(): <T>(task: () => Promise<T>) => Promise<T> {
  let tail: Promise<unknown> = Promise.resolve()
  return <T>(task: () => Promise<T>) => {
    const run = tail.then(task, task)
    tail = run.catch(() => undefined)
    return run
  }
}

/** いまの表示で、ファイル `staged` のチェックを切り替える本文（見つからなければ null） */
export function toggleFileIn(v: UploadView, staged: string): UploadPatch | null {
  for (const g of v.groups) {
    const t = g.tracks.find((m) => m.staged === staged)
    if (t != null) return fileTogglePatch(v.choices, t, true)
    const c = g.companions.find((m) => m.staged === staged)
    if (c != null) return fileTogglePatch(v.choices, c, false)
  }
  return null
}

/** いまの表示で、件 `key` のチェックを切り替える本文（見つからない・チェックできなければ null） */
export function toggleGroupIn(v: UploadView, key: string): UploadPatch | null {
  const g = v.groups.find((x) => x.key === key)
  if (g == null || !g.selectable) return null
  return groupTogglePatch(v.choices, g)
}

/** 受け取り終えた（外していない・壊れていない）同じパスの origin */
export function completedOrigin(v: UploadView | null, path: string): UploadOriginView | null {
  return v?.origins.find((o) => live(o) && o.complete && o.client_path === path) ?? null
}

/** 同じタブで送った（送っている）ファイルを、同じパスで足し直せるか: その origin を外した・壊れた */
export function canResend(send: { originId: string | null }, v: UploadView | null): boolean {
  if (send.originId == null || v == null) return false
  const o = v.origins.find((x) => x.id === send.originId)
  return o != null && (o.removed || o.damaged)
}
