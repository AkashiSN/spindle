// 再生の純粋ロジック（SPEC §11、D-52）: クライアント能力から stream の URL を決める、RG のゲイン、
// 次曲の決定、時刻の表示。DOM / Web Audio に触る部分は hooks/usePlayer にある

import type { RgValues, TrackRow } from '../api/types'

/** codec ごとにブラウザが再生できるか（起動時に canPlayType で判定） */
export type Support = Readonly<Record<'flac' | 'opus' | 'aac' | 'alac' | 'wav' | 'mp3', boolean>>

/** `canPlayType` の候補。'probably' / 'maybe' なら再生できるとみなす */
const PROBES: Record<keyof Support, string[]> = {
  flac: ['audio/flac', 'audio/x-flac'],
  opus: ['audio/ogg; codecs="opus"', 'audio/ogg'],
  aac: ['audio/mp4; codecs="mp4a.40.2"', 'audio/mp4'],
  alac: ['audio/mp4; codecs="alac"'],
  wav: ['audio/wav', 'audio/x-wav'],
  mp3: ['audio/mpeg'],
}

export function detectSupport(canPlayType: (mime: string) => string): Support {
  const out = {} as Record<keyof Support, boolean>
  for (const key of Object.keys(PROBES) as (keyof Support)[]) {
    out[key] = PROBES[key].some((m) => {
      const r = canPlayType(m)
      return r === 'probably' || r === 'maybe'
    })
  }
  return out
}

/** 表の codec → Support のキー。対応が無いものは null（原本の直送を試みる） */
function supportKey(codec: string): keyof Support | null {
  switch (codec) {
    case 'flac':
      return 'flac'
    case 'opus':
    case 'ogg':
      return 'opus'
    case 'aac':
      return 'aac'
    case 'alac':
      return 'alac'
    case 'wav':
    case 'aiff':
      return 'wav'
    case 'mp3':
      return 'mp3'
    default:
      return null
  }
}

export type Source = {
  url: string
  /** `transcode=opus`。Derived が無ければサーバが ffmpeg に倒す（Range 非対応の可能性） */
  transcode: boolean
}

/**
 * 再生に使う URL。可逆は既定で Derived の Opus（帯域）、`preferOriginal` なら原本。
 * ブラウザが再生できない codec は Opus に変換する（Opus も再生できなければ諦めて原本を試す）
 */
export function streamUrl(
  track: Pick<TrackRow, 'id' | 'codec' | 'lossless'>,
  support: Support,
  preferOriginal: boolean,
): Source {
  const base = `/api/stream/${track.id}`
  const key = supportKey(track.codec)
  const playable = key == null ? true : support[key]
  const transcode = track.lossless && ((!preferOriginal && support.opus) || !playable)
  return transcode ? { url: `${base}?transcode=opus`, transcode: true } : { url: base, transcode: false }
}

/** 変換ストリームを `start` 秒から読み直す URL */
export function withStart(source: Source, start: number): string {
  if (!source.transcode || start <= 0) return source.url
  return `${source.url}&start=${start.toFixed(3)}`
}

/**
 * RG のゲイン（線形）。`rg` は内部表現（-18 LUFS 基準の dB）。peak でクリップしないよう
 * `min(10^(gain/20), 1/peak)`。無効・未解析なら 1
 */
export function rgGain(rg: RgValues | null, enabled: boolean): number {
  if (!enabled || rg == null) return 1
  const linear = Math.pow(10, rg.track_gain / 20)
  if (!Number.isFinite(linear) || linear <= 0) return 1
  if (rg.track_peak > 0 && Number.isFinite(rg.track_peak)) return Math.min(linear, 1 / rg.track_peak)
  return linear
}

/**
 * 表の現在の順序で次に再生する行。`rows` は読み込み済みの行（先頭から連続）。現在曲が表に無ければ
 * null（フィルタ・ソートが変わった）。末尾まで来てまだ読んでいない行があれば（`exhausted` でない）
 * 'unloaded'（呼び出し側が ensure で読んでから判定し直す）
 */
export function nextTrack(
  rows: readonly TrackRow[],
  currentId: number,
  exhausted: boolean,
): TrackRow | 'unloaded' | null {
  const idx = rows.findIndex((r) => r.id === currentId)
  if (idx < 0) return null
  for (let i = idx + 1; i < rows.length; i++) {
    const r = rows[i]
    if (r.missing_since != null) continue
    return r
  }
  return exhausted ? null : 'unloaded'
}

/** 表の順で前の再生可能な行（missing は飛ばす）。先頭や表に無ければ null */
export function prevTrack(rows: readonly TrackRow[], currentId: number): TrackRow | null {
  const idx = rows.findIndex((r) => r.id === currentId)
  for (let i = idx - 1; i >= 0; i--) {
    if (rows[i].missing_since == null) return rows[i]
  }
  return null
}

/** `m:ss`（1 時間以上は `h:mm:ss`）。NaN / 負は `0:00` */
export function formatTime(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return '0:00'
  const s = Math.floor(seconds)
  const h = Math.floor(s / 3600)
  const m = Math.floor((s % 3600) / 60)
  const sec = s % 60
  const mm = h > 0 ? String(m).padStart(2, '0') : String(m)
  return `${h > 0 ? `${h}:` : ''}${mm}:${String(sec).padStart(2, '0')}`
}

/** metadata が来たときに、保留していたシークをどう適用するか */
export type PendingSeekAction = { kind: 'native'; currentTime: number } | { kind: 'reload'; start: number } | { kind: 'none' }

/**
 * `want` は保留していたシーク位置（表示上の秒）、`offset` は現在の読み直し開始位置。
 * Range でシークできる（`seekable`）なら currentTime（offset を引く）、chunked なら offset と
 * 実質同じ位置でない限り `start=want` で読み直す（前方だけでなく後方も）
 */
export function resolvePendingSeek(seekable: boolean, want: number | null, offset: number): PendingSeekAction {
  if (want == null) return { kind: 'none' }
  const w = Math.max(0, want)
  if (seekable) return { kind: 'native', currentTime: Math.max(0, w - offset) }
  if (Math.abs(w - offset) <= 1) return { kind: 'none' }
  return { kind: 'reload', start: w }
}
