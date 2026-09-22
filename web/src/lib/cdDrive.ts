// CD ドライブの状態（`GET /api/cd/status`、P2-1）の型と表示。ポーリングは hooks/useCdDrive.ts

import { normalizeTocInput } from './cd'

export type DriveState = 'unknown' | 'no_drive' | 'no_disc' | 'tray_open' | 'not_ready' | 'disc_ok'

export type DriveStatus = {
  state: DriveState
  /** ディスクがあって TOC を読めたら CTDB 形式（lookup にそのまま渡せる） */
  toc: string | null
  /** TOC と一緒に読んだ ISRC（音声トラック順。無いトラックは null）。TOC が無ければ空 */
  isrcs: Array<string | null>
  /** メディアカタログ番号（JAN / UPC）。入っていない盤は null */
  mcn: string | null
  error: string | null
  checked_at: number
}

/** TOC 文字列のトラック数（`:` 区切りの最後がリードアウト） */
function trackCount(toc: string): number {
  return toc.split(':').length - 1
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
      if (s.toc != null) return `ディスクあり（${trackCount(s.toc)} トラック）`
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
