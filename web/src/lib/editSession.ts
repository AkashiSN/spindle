// セルの編集 1 回分の確定 / 取り消しの判定（Inbox の承認画面。D-86）。
//
// 入力欄は Enter で確定、Esc で取り消し、フォーカスが外れたら確定する。Enter / Esc の処理でフォーカスを
// 表へ戻すと blur も起きるので、素直に書くと Esc で取り消した編集が blur で確定され、Enter は二重に確定
// される（codex 指摘）。1 回の編集で確定か取り消しは 1 回だけ、を ここで守る

export type EditAction = 'commit' | 'cancel' | null

export type EditSession = {
  /** 編集を始めた（前の編集の決着を忘れる） */
  start: () => void
  /** 入力欄のキー。Enter → commit、Escape → cancel、他は null */
  key: (key: string) => EditAction
  /** 入力欄からフォーカスが外れた。まだ決着していなければ commit */
  blur: () => EditAction
}

export function editSession(): EditSession {
  let settled = false
  const settle = (a: Exclude<EditAction, null>): EditAction => {
    if (settled) return null
    settled = true
    return a
  }
  return {
    start: () => {
      settled = false
    },
    key: (key) => (key === 'Enter' ? settle('commit') : key === 'Escape' ? settle('cancel') : null),
    blur: () => settle('commit'),
  }
}
