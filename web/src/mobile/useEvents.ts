// SSE /api/events を 1 本だけ開き、購読者へ配る（React context）。
// 各画面は useEventSubscription(handlers) で必要なイベントだけ受け取る

import { createContext, createElement, useContext, useEffect, useRef, useState, type ReactNode } from 'react'
import type { BatchEvent, JobEvent, LibraryEvent, PlaylistEvent } from '../api/types'
import { connectEvents } from '../lib/events'

export type MobileEventHandlers = {
  /** 接続が開いた。`reconnect` が true なら切断中の取りこぼしがあり得るので取り直す */
  onOpen?: (reconnect: boolean) => void
  onJob?: (e: JobEvent) => void
  /** 編集バッチの状態が変わった（反映待ち・競合の表示が変わる） */
  onBatch?: (e: BatchEvent) => void
  onLibrary?: (e: LibraryEvent) => void
  onPlaylist?: (e: PlaylistEvent) => void
}

type Subscribe = (get: () => MobileEventHandlers) => () => void

const EventsContext = createContext<Subscribe | null>(null)

/** `enabled` の間だけ接続する（ログイン前は開かない） */
export function EventsProvider({ enabled, children }: { enabled: boolean; children: ReactNode }) {
  const [subs] = useState(() => new Set<() => MobileEventHandlers>())
  const [subscribe] = useState<Subscribe>(() => (get: () => MobileEventHandlers) => {
    subs.add(get)
    return () => {
      subs.delete(get)
    }
  })
  useEffect(() => {
    if (!enabled) return
    const each = (f: (h: MobileEventHandlers) => void) => {
      for (const get of subs) f(get())
    }
    return connectEvents(() => ({
      onOpen: (reconnect) => each((h) => h.onOpen?.(reconnect)),
      onJob: (e) => each((h) => h.onJob?.(e)),
      onBatch: (e) => each((h) => h.onBatch?.(e)),
      onLibrary: (e) => each((h) => h.onLibrary?.(e)),
      onPlaylist: (e) => each((h) => h.onPlaylist?.(e)),
    }))
  }, [enabled, subs])
  return createElement(EventsContext.Provider, { value: subscribe }, children)
}

/** イベントを購読する。handlers は毎回差し替えてよい（最新のものが呼ばれる） */
export function useEventSubscription(handlers: MobileEventHandlers): void {
  const subscribe = useContext(EventsContext)
  const latest = useRef(handlers)
  useEffect(() => {
    latest.current = handlers
  })
  useEffect(() => {
    if (!subscribe) return
    return subscribe(() => latest.current)
  }, [subscribe])
}
