// 下から出すシート。半分（50dvh）と全体（92dvh）の 2 段。
// - つまみのタップで切り替え、ドラッグで上げ下げ（半分から下へ引けば閉じる）
// - 背景のタップで閉じる。戻るで閉じるのは呼び出し側（history.state.sheet）が受け持つ

import { useRef, useState, type CSSProperties, type PointerEvent, type ReactNode } from 'react'

/** これより動かしたらドラッグ（それ未満はタップ） */
const DRAG_THRESHOLD = 8
/** 段を変えるのに要る移動量（px） */
const SNAP = 60

export function BottomSheet({
  label,
  expanded,
  onExpandedChange,
  onClose,
  className,
  children,
}: {
  label: string
  expanded: boolean
  onExpandedChange?: (expanded: boolean) => void
  onClose: () => void
  className?: string
  children: ReactNode
}) {
  const start = useRef<{ y: number; id: number } | null>(null)
  const [drag, setDrag] = useState(0)
  const moved = useRef(false)

  const onPointerDown = (e: PointerEvent<HTMLElement>) => {
    start.current = { y: e.clientY, id: e.pointerId }
    moved.current = false
    e.currentTarget.setPointerCapture(e.pointerId)
  }
  const onPointerMove = (e: PointerEvent<HTMLElement>) => {
    const s = start.current
    if (!s || s.id !== e.pointerId) return
    const dy = e.clientY - s.y
    if (Math.abs(dy) >= DRAG_THRESHOLD) moved.current = true
    if (moved.current) setDrag(dy)
  }
  const onPointerUp = (e: PointerEvent<HTMLElement>) => {
    const s = start.current
    start.current = null
    setDrag(0)
    if (!s || s.id !== e.pointerId) return
    const dy = e.clientY - s.y
    if (!moved.current) {
      onExpandedChange?.(!expanded)
      return
    }
    if (dy <= -SNAP) onExpandedChange?.(true)
    else if (dy >= SNAP) {
      if (expanded && onExpandedChange) onExpandedChange(false)
      else onClose()
    }
  }
  const onPointerCancel = () => {
    start.current = null
    setDrag(0)
  }

  // ドラッグ中は指に追従させる。下へは沈める、上へは（半分のときだけ）全体まで伸ばす
  let style: CSSProperties | undefined
  if (drag > 0) style = { transform: `translateY(${drag}px)`, transition: 'none' }
  else if (drag < 0 && !expanded && onExpandedChange) style = { height: `calc(50dvh + ${-drag}px)`, transition: 'none' }

  return (
    <div className="m-sheet-layer">
      <div className="m-sheet-backdrop" onClick={onClose} />
      <section
        className={`m-sheet ${expanded ? 'full' : 'half'}${className ? ` ${className}` : ''}`}
        role="dialog"
        aria-modal="true"
        aria-label={label}
        style={style}
      >
        <button
          type="button"
          className="m-sheet-handle"
          aria-label={expanded ? '半分に戻す' : '引き上げる'}
          onPointerDown={onPointerDown}
          onPointerMove={onPointerMove}
          onPointerUp={onPointerUp}
          onPointerCancel={onPointerCancel}
          onClick={(e) => {
            // キーボードの Enter / Space（pointer を伴わない click）でも切り替える
            if (e.detail === 0) onExpandedChange?.(!expanded)
          }}
        >
          <span aria-hidden />
        </button>
        <div className="m-sheet-body">{children}</div>
      </section>
    </div>
  )
}
