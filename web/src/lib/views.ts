export type View = 'tracks' | 'albums' | 'cd' | 'jobs' | 'history' | 'settings'

/** 上部ナビの画面。「一覧」がホーム（SPEC §12.1） */
export const VIEWS: Array<[View, string]> = [
  ['tracks', '一覧'],
  ['albums', 'アルバム'],
  ['cd', 'CD'],
  ['jobs', 'ジョブ'],
  ['history', '履歴'],
  ['settings', '設定'],
]
