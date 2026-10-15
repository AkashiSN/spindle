import { expect, test } from 'vitest'
import { archiveEmptyText, archiveFilterTabs, RECORDS_TABS } from './records'

test('記録画面のタブは 編集履歴 / 退避ファイル / ログ の順', () => {
  expect(RECORDS_TABS).toEqual([
    ['history', '編集履歴'],
    ['archive', '退避ファイル'],
    ['logs', 'ログ'],
  ])
})

test('退避ファイルの切り替えは保持中が先頭で、すべては合計', () => {
  const tabs = archiveFilterTabs({ held: 0, restored: 2, deleted: 7570 })
  expect(tabs.map((t) => [t.key, t.label, t.count])).toEqual([
    ['held', '保持中', 0],
    ['restored', '復元済み', 2],
    ['deleted', '削除済み', 7570],
    ['all', 'すべて', 7572],
  ])
})

test('保持中が 0 件なら削除済みの件数を添えて説明する', () => {
  expect(archiveEmptyText('held', { held: 0, restored: 0, deleted: 7570 })).toBe(
    '保持中の退避ファイルは無い（削除済み 7,570 件は GC で実体を消した記録）',
  )
  expect(archiveEmptyText('held', { held: 0, restored: 0, deleted: 0 })).toBe('保持中の退避ファイルは無い')
  expect(archiveEmptyText('restored', { held: 0, restored: 0, deleted: 3 })).toBe('該当する退避ファイルは無い')
})
