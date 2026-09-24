// 番号付きの段（CD 画面と Inbox の承認画面の導線。§12.6）。見出しの右に要約（`aside`）と説明（`hint`。ⓘ）

import type { ReactNode } from 'react'
import { Hint } from './Hint'

/** 番号付きの段。見出しの右に要約（`aside`）と説明（`hint`） */
export function Step({
  no,
  title,
  aside,
  hint,
  children,
  done = false,
}: {
  no: number
  title: string
  aside?: ReactNode
  hint?: ReactNode
  children: ReactNode
  /** 終わった段（番号を緑にする） */
  done?: boolean
}) {
  return (
    <section className={done ? 'cd-step done' : 'cd-step'} aria-label={title}>
      <header className="cd-step-head">
        <span className="cd-step-no" aria-hidden="true">
          {no}
        </span>
        <h2>{title}</h2>
        {aside != null && <span className="cd-step-aside small">{aside}</span>}
        <span className="spacer" />
        {hint != null && <Hint align="right">{hint}</Hint>}
      </header>
      <div className="cd-step-body">{children}</div>
    </section>
  )
}
