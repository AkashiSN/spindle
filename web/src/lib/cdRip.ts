// CD の吸い出しの進捗（P2-5）。サーバの rip ジョブが SSE の `job` イベントの `detail` に載せる
// `RipProgress`（`src/cd/rip.rs`）を、トラック表の「進捗」欄と状態の一行にする。React に依存しない。
//
// 相の順序と単位（サーバと同じ）:
//   read   ドライブから全ディスクを 1 本で読む。done / total はセクタ。track_no は読んでいる位置のトラック
//   verify CRC を取って CTDB / AccurateRip と照合する。バイト。トラックに分かれない
//   repair CTDB のパリティで直す。バイト。トラックに分かれない
//   encode FLAC にする。トラック。track_no は **エンコードし終えた** トラック（done > 0 のとき）
//   place  Inbox に置く
// 照合が通らなければ read からやり直す（attempt が増える）。
//
// 表の「完了」の表現（P2-5 で確定）: 読み取り中は、読み終えた行に「読んだ」、読んでいる行に「読み取り中」。
// 照合・修復は全部の行に同じ言葉。エンコードは、終えた行に「FLAC 済」、次の行に「エンコード中」。
// 置いたら全部の行に「Inbox へ」

export type RipPhase = 'read' | 'verify' | 'repair' | 'encode' | 'place'

export type RipProgress = {
  phase: RipPhase
  /** 何回目の吸い出しか（1 始まり） */
  attempt: number
  /** 複数枚組のディスク番号（rip.log / disc<N>.cue と揃える。D-67） */
  disc_no: number
  /** いま扱っているトラック（読み取り・エンコード）。分からなければ null */
  track_no: number | null
  /** その相の完了数と総数 */
  done: number
  total: number
}

const PHASES: readonly RipPhase[] = ['read', 'verify', 'repair', 'encode', 'place']

/** `job` イベントの `detail` を読む。形が違えば null（他の種別のジョブ・古いサーバ） */
export function ripProgressFrom(detail: unknown): RipProgress | null {
  if (detail == null || typeof detail !== 'object') return null
  const d = detail as Record<string, unknown>
  const num = (v: unknown) => (typeof v === 'number' && Number.isFinite(v) ? v : null)
  if (typeof d.phase !== 'string' || !PHASES.includes(d.phase as RipPhase)) return null
  const done = num(d.done)
  const total = num(d.total)
  const attempt = num(d.attempt)
  const disc_no = num(d.disc_no)
  if (done == null || total == null || attempt == null || disc_no == null) return null
  return { phase: d.phase as RipPhase, attempt, disc_no, track_no: num(d.track_no), done, total }
}

/** トラック表の「進捗」欄。`number` はその行のトラック番号、`numbers` は表の全トラック番号（昇順） */
export function ripCellLabel(number: number, p: RipProgress, numbers: readonly number[]): string {
  switch (p.phase) {
    case 'read':
      if (p.track_no == null) return ''
      if (number < p.track_no) return '読んだ'
      if (number === p.track_no) return p.done >= p.total ? '読んだ' : '読み取り中'
      return ''
    case 'verify':
      return '照合中'
    case 'repair':
      return '修復中'
    case 'encode': {
      // track_no は終えたトラック（done が 0 の最初の報告は、まだどれも終えていない）
      const finished = p.done > 0 && p.track_no != null ? p.track_no : null
      if (finished != null && number <= finished) return 'FLAC 済'
      const next = numbers.find((n) => finished == null || n > finished)
      return number === next ? 'エンコード中' : ''
    }
    case 'place':
      return 'Inbox へ'
  }
}

const PHASE_LABELS: Record<RipPhase, string> = {
  read: '読み取り',
  verify: '照合',
  repair: 'CTDB で修復',
  encode: 'エンコード',
  place: 'Inbox に配置',
}

/** 状態の一行（「読み取り 45%」「照合 80%（2 回目）」） */
export function ripStatusLabel(p: RipProgress): string {
  const pct = p.total > 0 ? `${Math.min(100, Math.floor((p.done / p.total) * 100))}%` : ''
  const again = p.attempt > 1 ? `（${p.attempt} 回目。照合が通らないので吸い直している）` : ''
  const unit = p.phase === 'encode' && p.total > 0 ? ` ${p.done}/${p.total} 曲` : ''
  return `${PHASE_LABELS[p.phase]} ${pct}${unit}${again}`.replace(/\s+/g, ' ').trim()
}

/** 追っている rip ジョブの今の状態から、結果の一行を決める（`running` ならまだ終わっていない） */
export type RipOutcome = { kind: 'running' } | { kind: 'done'; result: string } | { kind: 'failed'; error: string }

export function ripOutcome(
  job: { state: string; note?: string | null; last_error: string | null } | undefined,
): RipOutcome {
  if (job?.state === 'queued' || job?.state === 'running') return { kind: 'running' }
  if (job?.state === 'done') return { kind: 'done', result: job.note ?? '取り込んだ（Inbox を見る）' }
  if (job?.state === 'cancelled') return { kind: 'failed', error: '取り消した' }
  // failed、または一覧から消えた（古いジョブの掃除）
  return { kind: 'failed', error: job?.last_error ?? '吸い出しに失敗した（ジョブ一覧を見る）' }
}

/** `POST /api/cd/rip` の失敗を人向けに */
export function ripErrorMessage(code: string, message: string): string {
  switch (code) {
    case 'duplicate':
      return '別の吸い出しが進んでいる（ドライブは 1 台）'
    case 'disc_mismatch':
      return 'ドライブの盤が変わった。照会し直してから取り込む'
    case 'bad_metadata':
      return `取り込めない: ${message}`
    case 'cd_unavailable':
      return 'CD ドライブが使えない'
    default:
      return message
  }
}
