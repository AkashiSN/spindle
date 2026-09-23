// CD ドライブの状態（`GET /api/cd/status`、P2-1）の型と表示。ポーリングは hooks/useCdDrive.ts

import type { TocTrackInfo } from './cd'
import { normalizeTocInput } from './cd'
import { formatDuration } from './format'

export type DriveState = 'unknown' | 'no_drive' | 'no_disc' | 'tray_open' | 'not_ready' | 'disc_ok'

export type DriveStatus = {
  state: DriveState
  /** ディスクがあって TOC を読めたら CTDB 形式（lookup にそのまま渡せる） */
  toc: string | null
  /** TOC の音声トラック（番号と長さ）。照会の前からトラック表を出すために使う。TOC が無ければ空 */
  tracks: TocTrackInfo[]
  /** TOC と一緒に読んだ ISRC（音声トラック順。無いトラックは null）。TOC が無ければ空 */
  isrcs: Array<string | null>
  /** メディアカタログ番号（JAN / UPC）。入っていない盤は null */
  mcn: string | null
  error: string | null
  checked_at: number
  /** 進行中（queued / running）の吸い出しジョブ（P2-5）。画面を開き直しても進捗を追う。旧サーバでは無い */
  rip_job?: number | null
}

/** セッション間隙（セクタ）。音声セッションの終端はデータトラック開始 − これ（SPEC §7.2、`cd/toc.rs`） */
const SESSION_GAP_SECTORS = 11400

/**
 * TOC 文字列（CTDB 形式の LBA 列。データトラックは `-` 前置、最後がリードアウト）から
 * 音声トラック数と音声区間の長さ。サーバの `Toc::audio_track_sectors` と同じ規則で、データトラックは
 * 数えず、最後の音声トラックの次がデータなら終端を 11400 セクタ手前にする。読めなければ null
 */
export function audioTocSummary(toc: string): { tracks: number; durationMs: number } | null {
  const parts = toc.split(':').map((p) => p.trim())
  if (parts.length < 2) return null
  const entries = parts.slice(0, -1).map((p) => ({ data: p.startsWith('-'), lba: Number(p.replace('-', '')) }))
  const leadout = Number(parts[parts.length - 1])
  if (!Number.isFinite(leadout) || entries.some((e) => !Number.isFinite(e.lba))) return null
  const audio = entries.filter((e) => !e.data)
  if (audio.length === 0) return null
  const lastAudio = entries.map((e) => e.data).lastIndexOf(false)
  const next = entries[lastAudio + 1]
  // 最後の音声トラックの次がデータトラックなら、音声の終端はその手前（Enhanced CD）
  const end = next?.data === true ? next.lba - SESSION_GAP_SECTORS : leadout
  const start = audio[0]!.lba
  if (end <= start) return null
  // 1 秒 = 75 セクタ
  return { tracks: audio.length, durationMs: ((end - start) * 1000) / 75 }
}

/** 状態の一行。まだ取れていなければ null */
export function driveStateLabel(s: DriveStatus | null): string | null {
  if (s == null) return null
  switch (s.state) {
    case 'unknown':
      return 'ドライブを確認中…'
    case 'no_drive':
      return s.error != null ? `ドライブが無い（${s.error}）` : 'ドライブが無い'
    case 'no_disc':
      return 'ディスクなし'
    case 'tray_open':
      return 'トレイが開いている'
    case 'not_ready':
      return 'ドライブの準備中…'
    case 'disc_ok':
      if (s.toc != null) {
        const summary = audioTocSummary(s.toc)
        if (summary == null) return 'ディスクあり'
        return `ディスクあり（${summary.tracks} トラック・${formatDuration(summary.durationMs)}）`
      }
      return s.error != null ? `ディスクあり（TOC を読めない: ${s.error}）` : 'ディスクあり（TOC を読み取り中…）'
  }
}

/** 照会に添える識別子（`useCdLookup` の `LookupExtra` と同じ形） */
export type DriveIds = { isrcs: Array<string | null>; mcn: string | null }

/**
 * 欄の TOC がいまドライブに入っている盤の TOC と同じときだけ、その盤の ISRC / MCN を返す。
 * 別の盤の TOC（貼り付け・編集後）に混ぜると、強い経路として誤同定するので空にする
 */
export function driveIdsFor(toc: string, s: DriveStatus | null): DriveIds {
  const empty: DriveIds = { isrcs: [], mcn: null }
  if (s == null || s.toc == null) return empty
  const normalized = normalizeTocInput(toc)
  if (normalized === '' || normalized !== s.toc) return empty
  return { isrcs: s.isrcs, mcn: s.mcn }
}

/**
 * 新しいディスクが入ったか。前回見た TOC（無ければ null）と違う TOC が出たときだけそれを返す。
 * 同じディスクの間・抜かれた後は null（ポーリングのたびに照会し直さない）
 */
export function newDiscToc(lastSeen: string | null, s: DriveStatus | null): string | null {
  const toc = s?.toc ?? null
  if (toc == null || toc === lastSeen) return null
  return toc
}
