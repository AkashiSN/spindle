// category の語彙（D-92）。スキャナが Library 直下のフォルダ名から語彙を足し、NULL の album を埋めるので、
// 開いている選択欄（useCategories）は `library` イベント（スキャンの変更）のたびに取り直す。
// React に依存しない部分: 取り直しの合図の購読と、削除の確認文

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
