// 番号付きの段（CD / Inbox / YouTube 画面とライブラリの一括編集・操作タブの導線。§12.6、D-87）。
// 見出しの右に要約（`aside`）と説明（`hint`。ⓘ）。状態は done（済み・緑）/ wait（前の段が済むまで淡く）/
// stale（済んだ結果が古い・枠を警告色）

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
  wait = false,
  stale = false,
  className,
}: {
  no: number
  title: string
  aside?: ReactNode
  hint?: ReactNode
  children: ReactNode
  /** 終わった段（番号を緑にする） */
  done?: boolean
  /** 前の段が済むまで待つ段（淡くする） */
  wait?: boolean
  /** 結果が古くなった段（枠を警告色にする） */
  stale?: boolean
  className?: string
}) {
  const cls = ['cd-step', done ? 'done' : '', wait ? 'wait' : '', stale ? 'stale' : '', className ?? '']
    .filter((c) => c !== '')
    .join(' ')
  return (
    <section className={cls} aria-label={title}>
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
