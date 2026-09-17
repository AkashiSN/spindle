// 再生（SPEC §11 / §12.1、D-52）。`<audio>` 要素 1 つを持ち、Web Audio の GainNode で RG を掛ける。
//
// - 再生する URL は lib/playback の streamUrl（canPlayType の結果と「可逆は原本を再生」設定）
// - `transcode=opus` を要求しても、サーバが Derived を直送したか ffmpeg の chunked かは応答からしか
//   分からない。`loadedmetadata` で duration が有限なら Range でネイティブにシークできる
//   （'seekable'）、無限 / NaN なら chunked（'chunked'。`start=` で読み直し、表示時刻を offset で
//   補正）。metadata 前のシークは保留して確定後に適用する
// - 次曲は表の現在の順序（lib/playback の nextTrack）。次が未読なら ensure で読み、届いたら続ける。
//   ユーザの play / stop は自動継続の待ちを必ず打ち切る。現在曲が表から消えていれば止まる

import { useCallback, useEffect, useRef, useState } from 'react'
import type { TrackRow } from '../api/types'
import {
  detectSupport,
  nextTrack,
  resolvePendingSeek,
  rgGain,
  streamUrl,
  withStart,
  type Source,
  type Support,
} from '../lib/playback'
import { useLocalStorageState } from './useLocalStorageState'

export type PlayerState = {
  track: TrackRow | null
  playing: boolean
  /** 表示上の再生位置（秒。変換ストリームでは offset 込み） */
  position: number
  /** 曲の長さ（秒）。不明なら null */
  duration: number | null
  /** chunked（オンザフライ変換）で再生中。シークは読み直し */
  transcoding: boolean
  error: string | null
  volume: number
  rgEnabled: boolean
  preferOriginal: boolean
  support: Support
}

export type PlayerHandle = PlayerState & {
  /** その行から再生する */
  play: (track: TrackRow) => void
  toggle: () => void
  stop: () => void
  seek: (seconds: number) => void
  setVolume: (v: number) => void
  setRgEnabled: (on: boolean) => void
  setPreferOriginal: (on: boolean) => void
}

type Rows = readonly TrackRow[]
type MediaState = 'loading' | 'seekable' | 'chunked'

const isBool = (v: unknown): v is boolean => typeof v === 'boolean'
const isVolume = (v: unknown): v is number => typeof v === 'number' && v >= 0 && v <= 1

export function usePlayer(rows: Rows, exhausted: boolean, ensure: (index: number) => void): PlayerHandle {
  // <audio> は描画に関係しない外部リソースなので ref に持つ（作るのは mount 時の effect）
  const audioRef = useRef<HTMLAudioElement | null>(null)
  const ctx = useRef<AudioContext | null>(null)
  const gain = useRef<GainNode | null>(null)
  const [support] = useState<Support>(() => {
    const probe = document.createElement('audio')
    return detectSupport((m) => probe.canPlayType(m))
  })

  const [track, setTrack] = useState<TrackRow | null>(null)
  const [source, setSource] = useState<Source | null>(null)
  const [playing, setPlaying] = useState(false)
  const [position, setPosition] = useState(0)
  const [duration, setDuration] = useState<number | null>(null)
  const [error, setError] = useState<string | null>(null)
  /** 応答の性質。metadata が来るまで 'loading' */
  const [media, setMedia] = useState<MediaState>('loading')
  /** metadata 前に要求されたシーク（秒。確定後に適用） */
  const pendingSeek = useRef<number | null>(null)
  /** 変換ストリームを `start=` で読み直したときの開始位置 */
  const offset = useRef(0)
  const [volume, setVolume] = useLocalStorageState<number>('player.volume', 1, isVolume)
  const [rgEnabled, setRgEnabled] = useLocalStorageState<boolean>('player.rg', true, isBool)
  const [preferOriginal, setPreferOriginal] = useLocalStorageState<boolean>('player.original', false, isBool)

  // 表の行は再生中に入れ替わる（ページの取り直し）。イベントハンドラからは最新を ref で見る
  const rowsRef = useRef(rows)
  const exhaustedRef = useRef(exhausted)
  const trackRef = useRef(track)
  const sourceRef = useRef(source)
  useEffect(() => {
    rowsRef.current = rows
    exhaustedRef.current = exhausted
    trackRef.current = track
    sourceRef.current = source
  })

  /** mount 時に <audio> を作る。unmount で止めて AudioContext も閉じる */
  useEffect(() => {
    const a = new Audio()
    a.preload = 'auto'
    audioRef.current = a
    return () => {
      a.pause()
      a.removeAttribute('src')
      a.load()
      audioRef.current = null
      if (gain.current) gain.current.disconnect()
      gain.current = null
      const c = ctx.current
      ctx.current = null
      if (c) void c.close().catch(() => {})
    }
  }, [])

  /** 初回の再生操作で AudioContext を作る（autoplay 制約で先に作ると suspended のまま） */
  const ensureGraph = useCallback(() => {
    const audio = audioRef.current
    if (!audio) return
    if (ctx.current) {
      if (ctx.current.state === 'suspended') void ctx.current.resume()
      return
    }
    try {
      const c = new AudioContext()
      const src = c.createMediaElementSource(audio)
      const g = c.createGain()
      src.connect(g)
      g.connect(c.destination)
      ctx.current = c
      gain.current = g
    } catch {
      // Web Audio が使えなければ素の <audio>（RG は掛からない）
    }
  }, [])

  // RG と音量
  useEffect(() => {
    const g = rgGain(track?.rg ?? null, rgEnabled)
    if (gain.current) gain.current.gain.value = g
    if (audioRef.current) audioRef.current.volume = volume
  }, [track, rgEnabled, volume])

  const fail = useCallback((e: unknown) => setError(e instanceof Error ? e.message : String(e)), [])

  const load = useCallback(
    (t: TrackRow, src: Source, start: number) => {
      const audio = audioRef.current
      if (!audio) return
      offset.current = start
      pendingSeek.current = null
      setMedia('loading')
      setSource(src)
      setTrack(t)
      setError(null)
      setPosition(start)
      setDuration(t.duration_ms != null ? t.duration_ms / 1000 : null)
      audio.src = withStart(src, start)
      audio.play().catch(fail)
    },
    [fail],
  )

  /** 曲が終わったとき次の行が未読だった（届いたら続ける）。ユーザの play / stop で打ち切る */
  const pendingNext = useRef(false)

  const play = useCallback(
    (t: TrackRow) => {
      pendingNext.current = false
      ensureGraph()
      load(t, streamUrl(t, support, preferOriginal), 0)
      // 次の行を先読みしておく（末尾で未読があれば取りに行く）
      const idx = rowsRef.current.findIndex((r) => r.id === t.id)
      if (idx >= 0 && idx + 1 >= rowsRef.current.length && !exhaustedRef.current) ensure(idx + 1)
    },
    [ensureGraph, load, support, preferOriginal, ensure],
  )

  const stop = useCallback(() => {
    pendingNext.current = false
    pendingSeek.current = null
    const audio = audioRef.current
    if (audio) {
      audio.pause()
      audio.removeAttribute('src')
      audio.load()
    }
    setTrack(null)
    setSource(null)
    setMedia('loading')
    setPlaying(false)
    setPosition(0)
    setDuration(null)
    offset.current = 0
  }, [])

  const toggle = useCallback(() => {
    const audio = audioRef.current
    if (!audio || !trackRef.current) return
    ensureGraph()
    if (audio.paused) audio.play().catch(fail)
    else audio.pause()
  }, [ensureGraph, fail])

  const seek = useCallback(
    (seconds: number) => {
      const audio = audioRef.current
      const t = trackRef.current
      if (!audio || !t || !source) return
      const s = Math.max(0, seconds)
      switch (media) {
        case 'loading':
          // 応答の性質が分かるまで保留（metadata で適用）
          pendingSeek.current = s
          setPosition(s)
          return
        case 'chunked':
          // Range 非対応: start= で読み直す
          load(t, source, s)
          return
        case 'seekable':
          audio.currentTime = s - offset.current
          return
      }
    },
    [media, source, load],
  )

  // <audio> のイベント
  useEffect(() => {
    const audio = audioRef.current
    if (!audio) return
    const onTime = () => setPosition(offset.current + audio.currentTime)
    const onPlay = () => setPlaying(true)
    const onPause = () => setPlaying(false)
    const onDuration = () => {
      if (Number.isFinite(audio.duration)) setDuration(offset.current + audio.duration)
    }
    const onMetadata = () => {
      // Derived / 原本（Range）は長さが分かる。ffmpeg の chunked は Infinity / NaN
      const seekable = Number.isFinite(audio.duration) && audio.duration > 0
      setMedia(seekable ? 'seekable' : 'chunked')
      const want = pendingSeek.current
      pendingSeek.current = null
      const action = resolvePendingSeek(seekable, want, offset.current)
      if (action.kind === 'native') {
        audio.currentTime = action.currentTime
      } else if (action.kind === 'reload') {
        const t = trackRef.current
        const src = sourceRef.current
        if (t && src) load(t, src, action.start)
      }
    }
    const onError = () => {
      const code = audio.error?.code
      setError(code != null ? `再生に失敗（code ${code}）` : '再生に失敗')
      setPlaying(false)
    }
    const onEnded = () => {
      const cur = trackRef.current
      if (!cur) return
      const next = nextTrack(rowsRef.current, cur.id, exhaustedRef.current)
      if (next === 'unloaded') {
        // 次の行がまだ来ていない。届いたら下の effect が再生する
        pendingNext.current = true
        setPlaying(false)
        return
      }
      if (next == null) {
        setPlaying(false)
        return
      }
      play(next)
    }
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
  }, [play, load])

  // 次曲が未読なら読んでおく（表が取り直された後にも）。終了時に未読だった次曲が届いたら続ける
  useEffect(() => {
    if (!track) return
    const idx = rows.findIndex((r) => r.id === track.id)
    if (idx >= 0 && idx + 1 >= rows.length && !exhausted) ensure(idx + 1)
    if (pendingNext.current) {
      const next = nextTrack(rows, track.id, exhausted)
      if (next === 'unloaded') return
      pendingNext.current = false
      // 描画中の setState を避けて次のティックで始める
      if (next != null) queueMicrotask(() => play(next))
    }
  }, [rows, exhausted, track, ensure, play])

  return {
    track,
    playing,
    position,
    duration,
    transcoding: media === 'chunked',
    error,
    volume,
    rgEnabled,
    preferOriginal,
    support,
    play,
    toggle,
    stop,
    seek,
    setVolume,
    setRgEnabled,
    setPreferOriginal,
  }
}
