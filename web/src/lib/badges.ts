// バッジ列の内容（SPEC §12.2）。表示は components/Badges.tsx

import type { TrackRow, Verification } from '../api/types'

const VERIFICATION: Record<Verification, { icon: string; label: string; cls: string }> = {
  verified_ar: { icon: '✔', label: '検証済み（AccurateRip）', cls: 'v-ar' },
  verified_ctdb: { icon: '✔', label: '検証済み（CTDB）', cls: 'v-ctdb' },
  mismatch: { icon: '✘', label: '検証不一致', cls: 'v-mismatch' },
  unverifiable: { icon: '○', label: '検証不能（TOC が無い音源）', cls: 'v-unverifiable' },
  not_attempted: { icon: '·', label: '未検証', cls: 'v-none' },
}

export type Badge = { key: string; icon: string; label: string; cls: string }

export function badgesOf(t: TrackRow): Badge[] {
  const out: Badge[] = []
  const v = VERIFICATION[t.verification] ?? VERIFICATION.not_attempted
  out.push({ key: 'verification', icon: v.icon, label: v.label, cls: `badge ${v.cls}` })
  out.push(
    t.lossless
      ? { key: 'lossless', icon: 'L', label: `可逆（${t.codec}）`, cls: 'badge b-lossless' }
      : { key: 'lossless', icon: 'l', label: `非可逆（${t.codec}）`, cls: 'badge b-lossy' },
  )
  if (t.rg_scanned_at != null) {
    const unwritten = t.rg_written_at == null || t.rg_written_at < t.rg_scanned_at
    out.push({
      key: 'rg',
      icon: 'RG',
      label: unwritten ? 'ReplayGain 計測済み（タグ未書き込み）' : 'ReplayGain 書き込み済み',
      cls: `badge b-rg${unwritten ? ' b-faded' : ''}`,
    })
  }
  if (t.derived) {
    out.push({
      key: 'derived',
      icon: t.derived.stale_tags ? 'D•' : 'D',
      label: t.derived.stale_tags
        ? `Derived あり（${t.derived.codec}、タグが古い）`
        : `Derived あり（${t.derived.codec}）`,
      cls: 'badge b-derived',
    })
  }
  if (t.flac_check && t.flac_check.status !== 'ok') {
    const stale = t.flac_check.stale ? '（結果が古い。再検査待ち）' : ''
    if (t.flac_check.status === 'decode_error') {
      out.push({
        key: 'flac',
        icon: t.flac_check.stale ? '✘F•' : '✘F',
        label: `FLAC のデコードエラー${stale}: ${t.flac_check.error ?? ''}`.trimEnd(),
        cls: 'badge b-flac-error',
      })
    } else {
      out.push({
        key: 'flac',
        icon: t.flac_check.stale ? 'F•' : 'F',
        label: `FLAC の STREAMINFO に MD5 が無い${stale}`,
        cls: 'badge b-flac-md5',
      })
    }
  }
  if (t.pending_batch_id != null) {
    out.push({
      key: 'pending',
      icon: '⏳',
      label: `反映待ち（バッチ #${t.pending_batch_id}）`,
      cls: 'badge b-pending',
    })
  }
  if (t.conflict_batch_id != null) {
    out.push({
      key: 'conflict',
      icon: '⚠',
      label: `conflict（バッチ #${t.conflict_batch_id} で外部変更と衝突）`,
      cls: 'badge b-conflict',
    })
  }
  if (t.duplicate_group) {
    out.push({ key: 'dup', icon: '⧉', label: '同一音声の重複がある', cls: 'badge b-dup' })
  }
  if (t.hardlink) {
    out.push({ key: 'hardlink', icon: '⛓', label: 'hardlink（nlink > 1）', cls: 'badge b-hardlink' })
  }
  if (t.missing_since != null) {
    out.push({ key: 'missing', icon: '∅', label: 'missing（ファイルが見つからない）', cls: 'badge b-missing' })
  }
  return out
}
