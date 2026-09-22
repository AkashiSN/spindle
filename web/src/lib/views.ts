export type View = 'tracks' | 'albums' | 'inbox' | 'cd' | 'youtube' | 'jobs' | 'history' | 'settings'

/**
 * 上部バーに並べる画面（SPEC §12.1、P4-20）。「ライブラリ」が日常の入口でホームなので先頭に置き、
 * 取り込みタブ（CD / YouTube）の右に Inbox を置く。取り込むと Inbox に赤い数字が増えるので、
 * 次に見る場所が分かる（タブ自体は ものが流れる順には並べない）
 */
export const VIEWS: Array<[View, string]> = [
  ['tracks', 'ライブラリ'],
  ['albums', 'アルバム'],
  ['cd', 'CD'],
  ['youtube', 'YouTube'],
  ['inbox', 'Inbox'],
]

/** ☰ メニューに入れる画面（たまにしか開かない） */
export const MENU_VIEWS: Array<[View, string]> = [
  ['jobs', 'ジョブ'],
  ['history', '履歴'],
  ['settings', '設定'],
]

/**
 * 左カラム（ツリー + アルバムアート）を出す画面。ツリーは表の絞り込み（scope）を差し替えるものなので、
 * 表のある画面だけに出す。CD / YouTube / Inbox / ジョブ / 履歴 / 設定では何にも効かないので畳む
 */
export const SIDEBAR_VIEWS: ReadonlySet<View> = new Set<View>(['tracks', 'albums'])

export function hasSidebar(v: View): boolean {
  return SIDEBAR_VIEWS.has(v)
}
