import { expect, test } from 'vitest'
import {
  archiveEmptyText,
  archiveFilterTabs,
  logAvailText,
  logKindLabel,
  logSummary,
  RECORDS_TABS,
  singleAlbumId,
} from './records'

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

test('ログの要約は disc ごとに CTDB → AR の順', () => {
  expect(
    logSummary([
      { disc_no: 1, method: 'ctdb', result: 'verified' },
      { disc_no: 1, method: 'accuraterip', result: 'not_found' },
      { disc_no: 2, method: 'ctdb', result: 'mismatch' },
    ]),
  ).toBe('disc 1: CTDB 一致 / AR 登録なし · disc 2: CTDB 不一致')
})

test('1 枚だけなら disc を書かない', () => {
  expect(logSummary([{ disc_no: 1, method: 'ctdb', result: 'unverifiable' }])).toBe('CTDB 検証不能')
  expect(logSummary([])).toBe('')
})

test('種別と本文の有無の文言', () => {
  expect(logKindLabel('retro')).toBe('照合')
  expect(logKindLabel('rip')).toBe('吸い出し')
  expect(logAvailText('latest')).toBeNull()
  expect(logAvailText('superseded')).toBe('このログは後の照合で上書きされた')
  expect(logAvailText('none')).toBe('ログは残っていない')
})

test('選択が 1 つの album のときだけその id', () => {
  expect(singleAlbumId([{ album_id: 3 }, { album_id: 3 }])).toBe(3)
  expect(singleAlbumId([{ album_id: 3 }, { album_id: 4 }])).toBeNull()
  expect(singleAlbumId([{ album_id: null }])).toBeNull()
  expect(singleAlbumId([])).toBeNull()
})
