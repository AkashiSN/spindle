// SSE /api/events の接続（SPEC §9、D-36、D-40）。React に依存しない部分。
//
// サーバの SSE は broadcast の購読で、event id による再送は無い。切断中に流れた
// job / batch / library は `resync` としても届かない（resync は接続中の購読者が遅れたとき専用）。
// なので **再接続のたびに一覧を取り直す**必要があり、初回の open と区別して呼び出し側へ伝える

import type { BatchEvent, JobEvent, LibraryEvent, PlaylistEvent, ResyncEvent } from '../api/types'

export type EventHandlers = {
  /** 接続が開いた。`reconnect` が true なら切断からの復帰（取りこぼしがあり得る） */
  onOpen?: (reconnect: boolean) => void
  onJob?: (e: JobEvent) => void
  onBatch?: (e: BatchEvent) => void
  onLibrary?: (e: LibraryEvent) => void
  onResync?: (e: ResyncEvent) => void
  /** プレイリストの項目が再評価で書き換わった（P1-7）。表示中なら表を取り直す */
  onPlaylist?: (e: PlaylistEvent) => void
  /** 切断（EventSource は自動で再接続を試みる）。401 で閉じられた場合もここに来る */
  onError?: () => void
}

/** EventSource と同じ形の最小インタフェース（テストで差し替える） */
export type EventSourceLike = {
  onopen: ((ev: Event) => void) | null
  onerror: ((ev: Event) => void) | null
  addEventListener(type: string, listener: (ev: Event) => void): void
  close(): void
}

function parse<T>(ev: Event): T | null {
  try {
    return JSON.parse((ev as MessageEvent).data as string) as T
  } catch {
    return null
  }
}

/**
 * 接続して振り分けを登録する。返り値で閉じる。
 * `get` は最新のハンドラを返す関数（React の再描画で差し替わってもリスナは付け直さない）
 */
export function connectEvents(
  get: () => EventHandlers,
  create: (url: string) => EventSourceLike = (url) => new EventSource(url),
): () => void {
  const es = create('/api/events')
  let opened = 0
  es.onopen = () => {
    opened += 1
    get().onOpen?.(opened > 1)
  }
  es.onerror = () => get().onError?.()
  function on<T>(name: string, f: (h: EventHandlers, e: T) => void) {
    es.addEventListener(name, (ev) => {
      const data = parse<T>(ev)
      if (data != null) f(get(), data)
    })
  }
  on<JobEvent>('job', (h, e) => h.onJob?.(e))
  on<BatchEvent>('batch', (h, e) => h.onBatch?.(e))
  on<LibraryEvent>('library', (h, e) => h.onLibrary?.(e))
  on<ResyncEvent>('resync', (h, e) => h.onResync?.(e))
  on<PlaylistEvent>('playlist', (h, e) => h.onPlaylist?.(e))
  return () => es.close()
}
