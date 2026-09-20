export type View = 'tracks' | 'albums' | 'inbox' | 'cd' | 'jobs' | 'history' | 'settings'

/** 上部ナビの画面。「一覧」がホーム（SPEC §12.1） */
/** 上部バーに並べる画面。「一覧」がホーム */
export const VIEWS: Array<[View, string]> = [
  ['tracks', '一覧'],
  ['albums', 'アルバム'],
  ['inbox', 'Inbox'],
  ['cd', 'CD'],
  ['jobs', 'ジョブ'],
]

/** ☰ メニューに入れる画面（たまにしか開かない） */
export const MENU_VIEWS: Array<[View, string]> = [
  ['history', '履歴'],
  ['settings', '設定'],
]
