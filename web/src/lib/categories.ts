// category の語彙（D-92）。スキャナが Library 直下のフォルダ名から語彙を足し、NULL の album を埋めるので、
// 開いている選択欄（useCategories）は `library` イベント（スキャンの変更）のたびに取り直す。
// React に依存しない部分: 取り直しの合図の購読と、削除の確認文・使用の文言

import { formatCount } from './format'

/** 統制語彙の 1 件（GET /api/categories は使用数つき。追加の直後の行には使用数が無い = 0） */
export type Category = {
  id: number
  name: string
  /** 使用数（GET /api/categories。追加の直後の行には無い = 0） */
  albums?: number
  genres?: number
  subscriptions?: number
  inbox_drafts?: number
}

type Listener = () => void

const listeners = new Set<Listener>()

/** 語彙が変わったかもしれない（スキャンの変更・削除）。購読中の一覧に取り直しを促す */
export function notifyCategoriesChanged(): void {
  for (const l of [...listeners]) l()
}

/** 取り直しの合図を購読する。返り値で解除 */
export function onCategoriesChanged(l: Listener): () => void {
  listeners.add(l)
  return () => {
    listeners.delete(l)
  }
}

/** 削除の確認ダイアログの文言 */
export function deleteConfirmText(name: string): string {
  return (
    `category「${name}」を削除する？（使われていない語彙だけ消せる。` +
    `Library の直下に同じ名前のフォルダがあってアルバムが入っていれば、次のスキャンでまた作られる）`
  )
}

/** album 以外の使用の内訳（GENRE の写像・購読・Inbox の下書き。0 の項目は出さない） */
function otherParts(c: Category): string[] {
  const parts: string[] = []
  const genres = c.genres ?? 0
  const subscriptions = c.subscriptions ?? 0
  const drafts = c.inbox_drafts ?? 0
  if (genres > 0) parts.push(`GENRE の写像 ${formatCount(genres)}`)
  if (subscriptions > 0) parts.push(`購読 ${formatCount(subscriptions)}`)
  if (drafts > 0) parts.push(`Inbox の下書き ${formatCount(drafts)}`)
  return parts
}

/** album 以外の使用（表の列） */
export function otherUsageText(c: Category): string {
  return otherParts(c).join('・')
}

/** 削除できない理由（削除ボタンの横）。未使用なら null */
export function categoryUsageText(c: Category): string | null {
  const albums = c.albums ?? 0
  const parts = [...(albums > 0 ? [`${formatCount(albums)} album`] : []), ...otherParts(c)]
  return parts.length === 0 ? null : `${parts.join('・')} で使用中`
}
