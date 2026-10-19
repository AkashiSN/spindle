// 戻るで閉じるシート（名前・確認・プレイリストに追加など）。曲のシート（App の useSheet）と同じく
// history.state に印を積み、戻る（popstate）で印が消えたら閉じる。UI からの「閉じる」も history を戻す。
// - 印は history.state.overlay（文字列の配列）。曲のシートの { sheet, edit } は引き継ぐので、
//   曲のシートの上に開いても、戻ると曲のシートは開いたまま
// - URL は変えない（useRoute は同じ URL の popstate を無視する）

import { useCallback, useEffect, useRef, useState } from 'react'

/** history.state に積んだシートの印（下から順） */
export function overlaysOf(state: unknown): string[] {
  if (state == null || typeof state !== 'object') return []
  const o = (state as { overlay?: unknown }).overlay
  return Array.isArray(o) ? o.filter((x): x is string => typeof x === 'string') : []
}

/** 今の state に印を 1 つ足したもの（他のキーは引き継ぐ） */
export function withOverlay(state: unknown, token: string): Record<string, unknown> {
  const base = state != null && typeof state === 'object' ? (state as Record<string, unknown>) : {}
  return { ...base, overlay: [...overlaysOf(state), token] }
}

let seq = 0

/**
 * 開いている間 history に 1 段積み、戻るで onClose を呼ぶ。返す関数で UI から閉じる
 * （積んだ段を戻し、popstate で onClose → after の順に呼ぶ）。
 * 閉じた後に画面を移る（navigate で積む）なら after に渡す。戻る前に積むと、戻るで移った先が消える
 */
export function useBackClose(onClose: () => void): (after?: () => void) => void {
  const [token] = useState(() => `o${Date.now().toString(36)}-${++seq}`)
  const onCloseRef = useRef(onClose)
  const afterRef = useRef<(() => void) | null>(null)
  const closed = useRef(false)
  useEffect(() => {
    onCloseRef.current = onClose
  })

  useEffect(() => {
    // StrictMode の effect の二度走りでは既に積んである
    if (!overlaysOf(window.history.state).includes(token))
      window.history.pushState(withOverlay(window.history.state, token), '', window.location.href)
    const onPop = () => {
      if (closed.current || overlaysOf(window.history.state).includes(token)) return
      closed.current = true
      const after = afterRef.current
      afterRef.current = null
      onCloseRef.current()
      after?.()
    }
    window.addEventListener('popstate', onPop)
    return () => window.removeEventListener('popstate', onPop)
  }, [token])

  return useCallback(
    (after?: () => void) => {
      if (closed.current) return
      const ov = overlaysOf(window.history.state)
      const i = ov.indexOf(token)
      if (i >= 0) {
        afterRef.current = after ?? null
        window.history.go(-(ov.length - i))
      } else {
        // 積んだ段が既に無い（別の画面へ移った後など）。その場で閉じる
        closed.current = true
        onCloseRef.current()
        after?.()
      }
    },
    [token],
  )
}
