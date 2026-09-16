// SSE /api/events の購読を React に繋ぐ。本体は lib/events.ts（テスト可能な純粋部分）。
// 一覧の取得は SSE を開いてから行う（逆順だと開く前のイベントを失う。D-36）ので、
// 初回の `onOpen(false)` を初回取得のトリガに、`onOpen(true)`（再接続）を全体の取り直しに使う

import { useEffect, useRef } from 'react'
import { connectEvents, type EventHandlers } from '../lib/events'

export type { EventHandlers } from '../lib/events'

export function useEvents(handlers: EventHandlers, enabled = true) {
  const ref = useRef(handlers)
  useEffect(() => {
    ref.current = handlers
  })
  useEffect(() => {
    if (!enabled) return
    return connectEvents(() => ref.current)
  }, [enabled])
}
