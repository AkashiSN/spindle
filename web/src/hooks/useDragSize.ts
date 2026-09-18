// ペインの境界をドラッグして大きさを変える（幅か高さ）。値は localStorage に永続化し、
// CSS 変数で親グリッドへ伝える（P1-12、D-58）

import { useEffect, useRef, type MouseEvent } from 'react'
import { useLocalStorageState } from './useLocalStorageState'

export type DragAxis = 'x' | 'y'

export function useDragSize(opts: {
  key: string
  cssVar: string
  fallback: number
  min: number
  max: number
  axis: DragAxis
  /** 境界をプラス方向へ動かしたとき大きさが増えるなら 1、減るなら -1 */
  direction: 1 | -1
}): { size: number; onMouseDown: (e: MouseEvent) => void } {
  const { key, cssVar, fallback, min, max, axis, direction } = opts
  const [size, setSize] = useLocalStorageState<number>(
    key,
    fallback,
    (v): v is number => typeof v === 'number' && v >= min && v <= max,
  )
  useEffect(() => {
    document.documentElement.style.setProperty(cssVar, `${size}px`)
  }, [cssVar, size])
  const drag = useRef<{ start: number; startSize: number } | null>(null)
  const onMouseDown = (e: MouseEvent) => {
    e.preventDefault()
    drag.current = { start: axis === 'x' ? e.clientX : e.clientY, startSize: size }
    const move = (ev: globalThis.MouseEvent) => {
      if (!drag.current) return
      const pos = axis === 'x' ? ev.clientX : ev.clientY
      const next = drag.current.startSize + direction * (pos - drag.current.start)
      setSize(Math.min(max, Math.max(min, next)))
    }
    const up = () => {
      drag.current = null
      window.removeEventListener('mousemove', move)
      window.removeEventListener('mouseup', up)
    }
    window.addEventListener('mousemove', move)
    window.addEventListener('mouseup', up)
  }
  return { size, onMouseDown }
}
