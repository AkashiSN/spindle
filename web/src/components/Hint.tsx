// 説明文のツールチップ（ⓘ）。本文に並べると画面の流れが読めなくなる長い説明を、ホバーかフォーカスで
// 出す。キーボードでも読めるよう tabIndex を持ち、読み上げは aria-describedby で本文に結ぶ。
// `align` は吹き出しの伸びる向き: 行の右端に置くなら 'right'（左へ伸びる）、行の途中なら 'left'

import { useId, type ReactNode } from 'react'

export function Hint({
  children,
  label = 'ⓘ',
  align = 'left',
}: {
  children: ReactNode
  label?: ReactNode
  align?: 'left' | 'right'
}) {
  const id = useId()
  return (
    <span className={`hint hint-${align}`} tabIndex={0} aria-describedby={id}>
      <span className="hint-mark" aria-hidden="true">
        {label}
      </span>
      <span className="hint-tip" role="tooltip" id={id}>
        {children}
      </span>
    </span>
  )
}
