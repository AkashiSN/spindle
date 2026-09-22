import { expect, test } from 'vitest'
import { hasSidebar, MENU_VIEWS, VIEWS, type View } from './views'

test('ライブラリが先頭で、取り込みタブの右に Inbox が来る', () => {
  // 取り込んだら右隣の Inbox に赤い数字が増える、という導線（SPEC §12.1、P4-20）。
  // ものは 入力 → Inbox → ライブラリ と流れるが、タブは流れ順には並べない
  expect(VIEWS.map(([v]) => v)).toEqual(['tracks', 'albums', 'cd', 'youtube', 'inbox'])
  expect(VIEWS[0]?.[1]).toBe('ライブラリ')
  expect(MENU_VIEWS.map(([v]) => v)).toEqual(['jobs', 'history', 'settings'])
})

test('左カラムはツリーの選択が表に効く画面だけ', () => {
  expect(hasSidebar('tracks')).toBe(true)
  expect(hasSidebar('albums')).toBe(true)
  const without: View[] = ['cd', 'youtube', 'inbox', 'jobs', 'history', 'settings']
  for (const v of without) expect(hasSidebar(v)).toBe(false)
})
