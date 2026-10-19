// スマホ版の試し聴き。<audio> を 1 つ持ち、mobileSource の URL（可逆は Opus / AAC に変換）を鳴らす。
// - RG は掛けない（試し聴きなので。Web Audio も使わない）
// - キュー・次曲は無い。曲が終われば止まる
// - Derived の直送（Range あり）は duration が有限なのでネイティブにシーク。ffmpeg の chunked
//   （Infinity / NaN）は `start=` で読み直し、表示時刻を offset で補正する（PC の usePlayer と同じ考え方）

import { useCallback, useEffect, useRef, useState } from 'react'
import type { TrackRow } from '../api/types'
import { detectSupport, resolvePendingSeek, withStart, type Source, type Support } from '../lib/playback'
import { mobileSource } from './track'

type MediaState = 'loading' | 'seekable' | 'chunked'

export type PreviewPlayer = {
  current: TrackRow | null
  playing: boolean
  /** 表示上の再生位置（秒。読み直したときは offset 込み） */
  position: number
  /** 曲の長さ（秒）。不明なら 0 */
  duration: number
  /** 再生に失敗したときの文言 */
  error: string | null
  /** 端末が Opus を鳴らせるか（鳴らせなければ AAC で流す） */
  codec: 'opus' | 'aac'
  play(row: TrackRow): void
  toggle(): void
  seek(seconds: number): void
}

export function usePreviewPlayer(): PreviewPlayer {
  const audioRef = useRef<HTMLAudioElement | null>(null)
  const [support] = useState<Support>(() => {
    const probe = document.createElement('audio')
    return detectSupport((m) => probe.canPlayType(m))
  })
  const [current, setCurrent] = useState<TrackRow | null>(null)
  const [playing, setPlaying] = useState(false)
  const [position, setPosition] = useState(0)
  const [duration, setDuration] = useState(0)
  const [error, setError] = useState<string | null>(null)
  const media = useRef<MediaState>('loading')
  const source = useRef<Source | null>(null)
  const offset = useRef(0)
  const pendingSeek = useRef<number | null>(null)

  useEffect(() => {
    const a = new Audio()
    a.preload = 'auto'
    audioRef.current = a
    return () => {
      a.pause()
      a.removeAttribute('src')
      a.load()
      audioRef.current = null
    }
  }, [])

  const fail = useCallback((e: unknown) => {
    // 次の読み直し・曲の切り替えで中断された play() は失敗ではない
    if (e instanceof DOMException && e.name === 'AbortError') return
    setError(e instanceof Error ? e.message : String(e))
  }, [])

  const load = useCallback(
    (src: Source, start: number) => {
      const audio = audioRef.current
      if (!audio) return
      source.current = src
      offset.current = start
      pendingSeek.current = null
      media.current = 'loading'
      setError(null)
      setPosition(start)
      audio.src = withStart(src, start)
      audio.play().catch(fail)
    },
    [fail],
  )

  const play = useCallback(
    (row: TrackRow) => {
      setCurrent(row)
      setDuration(row.duration_ms != null ? row.duration_ms / 1000 : 0)
      load(mobileSource(row, support), 0)
    },
    [load, support],
  )

  const toggle = useCallback(() => {
    const audio = audioRef.current
    if (!audio || source.current == null) return
    if (audio.paused) audio.play().catch(fail)
    else audio.pause()
  }, [fail])

  const seek = useCallback(
    (seconds: number) => {
      const audio = audioRef.current
      const src = source.current
      if (!audio || !src) return
      const s = Math.max(0, seconds)
      switch (media.current) {
        case 'loading':
          pendingSeek.current = s
          setPosition(s)
          return
        case 'chunked':
          load(src, s)
          return
        case 'seekable':
          audio.currentTime = s - offset.current
          return
      }
    },
    [load],
  )

  useEffect(() => {
    const audio = audioRef.current
    if (!audio) return
    const onTime = () => setPosition(offset.current + audio.currentTime)
    const onPlay = () => setPlaying(true)
    const onPause = () => setPlaying(false)
    const onDuration = () => {
      if (Number.isFinite(audio.duration) && audio.duration > 0) setDuration(offset.current + audio.duration)
    }
    const onMetadata = () => {
      const seekable = Number.isFinite(audio.duration) && audio.duration > 0
      media.current = seekable ? 'seekable' : 'chunked'
      const want = pendingSeek.current
      pendingSeek.current = null
      const action = resolvePendingSeek(seekable, want, offset.current)
      if (action.kind === 'native') audio.currentTime = action.currentTime
      else if (action.kind === 'reload' && source.current) load(source.current, action.start)
    }
    const onError = () => {
      const code = audio.error?.code
      setError(code != null ? `再生に失敗しました（code ${code}）` : '再生に失敗しました')
      setPlaying(false)
    }
    const onEnded = () => setPlaying(false)
    audio.addEventListener('timeupdate', onTime)
    audio.addEventListener('play', onPlay)
    audio.addEventListener('pause', onPause)
    audio.addEventListener('durationchange', onDuration)
    audio.addEventListener('loadedmetadata', onMetadata)
    audio.addEventListener('error', onError)
    audio.addEventListener('ended', onEnded)
    return () => {
      audio.removeEventListener('timeupdate', onTime)
      audio.removeEventListener('play', onPlay)
      audio.removeEventListener('pause', onPause)
      audio.removeEventListener('durationchange', onDuration)
      audio.removeEventListener('loadedmetadata', onMetadata)
      audio.removeEventListener('error', onError)
      audio.removeEventListener('ended', onEnded)
    }
  }, [load])

  return {
    current,
    playing,
    position,
    duration,
    error,
    codec: support.opus ? 'opus' : 'aac',
    play,
    toggle,
    seek,
  }
}
