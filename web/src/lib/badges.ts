// バッジ列の内容（SPEC §12.2）。表示は components/Badges.tsx

import type { HiresCheck, TrackRow, Verification } from '../api/types'

export const HIRES_LABEL: Record<HiresCheck['status'], string> = {
  ok: 'OK',
  upsampled: 'アップサンプリングの疑い',
  padded: 'ビット深度の水増し',
  both: 'アップサンプリングとビット深度の水増しの疑い',
  inconclusive: '判定できず',
  decode_error: 'デコードエラー',
}

/** 「カットオフ 22.1 kHz / 崖 48 dB / 実効 24 bit」。無い値は省く */
export function hiresMeasurements(h: HiresCheck): string {
  const parts: string[] = []
  if (h.cutoff_hz != null) parts.push(`カットオフ ${(h.cutoff_hz / 1000).toFixed(1)} kHz`)
  if (h.cliff_db != null) parts.push(`崖 ${Math.round(h.cliff_db)} dB`)
  if (h.effective_bits != null) parts.push(`実効 ${h.effective_bits} bit`)
  return parts.join(' / ')
}

export const VERIFICATION: Record<Verification, { icon: string; label: string; cls: string }> = {
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
  // D バッジは配布ビューの opus 系統だけ（aac 系統はプロパティで見せる。SPEC §7.6）
  const opus = t.derived.opus
  if (opus) {
    out.push({
      key: 'derived',
      icon: opus.stale_tags ? 'D•' : 'D',
      label: opus.stale_tags
        ? `Derived あり（${opus.codec}、タグが古い）`
        : `Derived あり（${opus.codec}）`,
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
  if (t.hires_check && t.hires_check.status !== 'ok') {
    const h = t.hires_check
    const stale = h.stale ? '（結果が古い。再検査待ち）' : ''
    const icon = h.stale ? 'H•' : 'H'
    if (h.status === 'decode_error') {
      out.push({
        key: 'hires',
        icon,
        label: `偽ハイレゾ検出: デコードエラー${stale}: ${h.error ?? ''}`.trimEnd(),
        cls: 'badge b-hires-error',
      })
    } else if (h.status === 'inconclusive') {
      out.push({
        key: 'hires',
        icon,
        label: `偽ハイレゾ検出: 判定できず（${hiresMeasurements(h)}）${stale}`,
        cls: 'badge b-hires-inconclusive',
      })
    } else {
      out.push({
        key: 'hires',
        icon,
        label: `${HIRES_LABEL[h.status]}（${hiresMeasurements(h)}）${stale}`,
        cls: 'badge b-hires-suspect',
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

/** 凡例の 1 行。`key` は badgesOf が出す key と対応する（テストで突き合わせる） */
export type LegendEntry = { key: string; icon: string; cls: string; label: string }

/** バッジの凡例（表の「凡例」ボタン）。badgesOf の各バッジを、出る条件ごとに 1 行ずつ */
export const BADGE_LEGEND: ReadonlyArray<{ title: string; items: LegendEntry[] }> = [
  {
    title: '検証（CD 由来の照合。常に出る）',
    items: [
      { key: 'verification', icon: '✔', cls: 'badge v-ar', label: '検証済み（AccurateRip）' },
      { key: 'verification', icon: '✔', cls: 'badge v-ctdb', label: '検証済み（CTDB）' },
      { key: 'verification', icon: '✘', cls: 'badge v-mismatch', label: '検証不一致' },
      { key: 'verification', icon: '○', cls: 'badge v-unverifiable', label: '検証不能（TOC が無い音源。配信・ダウンロード）' },
      { key: 'verification', icon: '·', cls: 'badge v-none', label: '未検証' },
    ],
  },
  {
    title: '形式（常に出る）',
    items: [
      { key: 'lossless', icon: 'L', cls: 'badge b-lossless', label: '可逆（FLAC / ALAC / WAV …）' },
      { key: 'lossless', icon: 'l', cls: 'badge b-lossy', label: '非可逆（Opus / MP3 / AAC …）' },
    ],
  },
  {
    title: 'ReplayGain / Derived',
    items: [
      { key: 'rg', icon: 'RG', cls: 'badge b-rg', label: 'ReplayGain 書き込み済み' },
      { key: 'rg', icon: 'RG', cls: 'badge b-rg b-faded', label: 'ReplayGain 計測済み（タグ未書き込み。薄い表示）' },
      { key: 'derived', icon: 'D', cls: 'badge b-derived', label: 'Derived（配布用の Opus）あり' },
      { key: 'derived', icon: 'D•', cls: 'badge b-derived', label: 'Derived あり、タグが古い（追随待ち）' },
    ],
  },
  {
    title: 'FLAC 検査 / 偽ハイレゾ検出（問題があるときだけ出る。• は結果が古い）',
    items: [
      { key: 'flac', icon: 'F', cls: 'badge b-flac-md5', label: 'FLAC の STREAMINFO に MD5 が無い' },
      { key: 'flac', icon: '✘F', cls: 'badge b-flac-error', label: 'FLAC のデコードエラー' },
      { key: 'hires', icon: 'H', cls: 'badge b-hires-suspect', label: 'アップサンプリング / ビット深度の水増しの疑い' },
      { key: 'hires', icon: 'H', cls: 'badge b-hires-inconclusive', label: '偽ハイレゾ検出: 判定できず（計測値を見る）' },
      { key: 'hires', icon: 'H', cls: 'badge b-hires-error', label: '偽ハイレゾ検出: デコードエラー' },
    ],
  },
  {
    title: '状態',
    items: [
      { key: 'pending', icon: '⏳', cls: 'badge b-pending', label: '編集の反映待ち（バッチ）' },
      { key: 'conflict', icon: '⚠', cls: 'badge b-conflict', label: 'conflict（外部変更と衝突して反映されなかった）' },
      { key: 'dup', icon: '⧉', cls: 'badge b-dup', label: '同一音声の重複がある' },
      { key: 'hardlink', icon: '⛓', cls: 'badge b-hardlink', label: 'hardlink（nlink > 1）' },
      { key: 'missing', icon: '∅', cls: 'badge b-missing', label: 'missing（ファイルが見つからない）' },
    ],
  },
]

