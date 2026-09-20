import { describe, expect, it } from 'vitest'
import type { AlbumRow } from '../api/types'
import { buildTree, EMPTY_LABEL, parsePattern, PRESETS, renderLevel, TREE_FIELDS } from './tree'

function album(over: Partial<AlbumRow> & { id: number }): AlbumRow {
  return {
    rel_dir: `dir/${over.id}`,
    category: null,
    albumartist: null,
    album: null,
    date: null,
    original_date: null,
    edition: null,
    mb_release_id: null,
    disc_count: null,
    artwork_id: null,
    artwork_hash: null,
    track_count: 1,
    duration_ms: 0,
    missing_since: null,
  album_gain: false,
    ...over,
  }
}

describe('parsePattern', () => {
  it('| で階層、%field% で差し込み、[ ] で省略可能な部分', () => {
    const p = parsePattern('[%year%] %albumartist%|%album%')
    expect('error' in p).toBe(false)
    if ('error' in p) return
    expect(p.levels).toHaveLength(2)
    expect(p.levels[0]).toEqual([
      { kind: 'optional', tokens: [{ kind: 'field', field: 'year' }] },
      { kind: 'text', text: ' ' },
      { kind: 'field', field: 'albumartist' },
    ])
    expect(p.levels[1]).toEqual([{ kind: 'field', field: 'album' }])
  })

  it('構文エラーは位置付きで返す', () => {
    expect(parsePattern('%album')).toMatchObject({ error: expect.stringContaining('%') })
    expect(parsePattern('%genre%')).toMatchObject({ error: expect.stringContaining('genre') })
    expect(parsePattern('[%album%')).toMatchObject({ error: expect.stringContaining('[') })
    expect(parsePattern('')).toMatchObject({ error: expect.anything() })
    expect(parsePattern('%album%|')).toMatchObject({ error: expect.anything() })
  })

  it('プリセットは全て有効で、フィールド一覧は固定', () => {
    for (const p of PRESETS) expect('error' in parsePattern(p.pattern)).toBe(false)
    expect(TREE_FIELDS).toContain('year')
    expect(TREE_FIELDS).toContain('rel_dir')
    expect(TREE_FIELDS).toContain('folder')
    expect(PRESETS[0].pattern).toBe('%folder%')
  })
})

describe('renderLevel', () => {
  it('値を差し込み、[ ] は条件記号で表示されず、中のフィールドが全部空なら丸ごと消える', () => {
    const p = parsePattern('[%year% - ]%albumartist% — %album%')
    if ('error' in p) throw new Error(p.error)
    expect(renderLevel(p.levels[0], album({ id: 1, date: '2012-07-25', albumartist: 'A', album: 'X' }))).toBe(
      '2012 - A — X',
    )
    expect(renderLevel(p.levels[0], album({ id: 1, albumartist: 'A', album: 'X' }))).toBe('A — X')
  })

  it('year は date の先頭 4 桁、無ければ空', () => {
    const p = parsePattern('%year%')
    if ('error' in p) throw new Error(p.error)
    expect(renderLevel(p.levels[0], album({ id: 1, date: '1999' }))).toBe('1999')
    expect(renderLevel(p.levels[0], album({ id: 1, date: '12' }))).toBe('')
    expect(renderLevel(p.levels[0], album({ id: 1 }))).toBe('')
  })
})

describe('buildTree', () => {
  const albums = [
    album({ id: 1, category: 'J-Pop', albumartist: 'A', album: 'X', track_count: 3 }),
    album({ id: 2, category: 'J-Pop', albumartist: 'A', album: 'Y', track_count: 2 }),
    album({ id: 3, category: 'J-Pop', albumartist: 'B', album: 'Z', track_count: 1 }),
    album({ id: 4, category: 'Game', albumartist: 'C', album: 'W', track_count: 4 }),
    album({ id: 5, albumartist: 'C', album: 'V', track_count: 1 }), // category 無し
    album({ id: 6, category: 'Game', albumartist: 'C', album: 'Gone', track_count: 9, missing_since: 1 }),
  ]

  it('by category: 階層ごとに集約し、件数と配下の album id を持つ。missing は除く', () => {
    const p = parsePattern('%category%|%albumartist%|%album%')
    if ('error' in p) throw new Error(p.error)
    const tree = buildTree(albums, p)
    expect(tree.map((n) => [n.label, n.count])).toEqual([
      ['Game', 4],
      ['J-Pop', 6],
      ['（なし）', 1],
    ])
    const jpop = tree[1]
    expect(jpop.albumIds).toEqual([1, 2, 3])
    expect(jpop.children.map((n) => [n.label, n.count])).toEqual([
      ['A', 5],
      ['B', 1],
    ])
    const a = jpop.children[0]
    expect(a.children.map((n) => [n.label, n.count, n.albumIds])).toEqual([
      ['X', 3, [1]],
      ['Y', 2, [2]],
    ])
    expect(a.children[0].children).toEqual([])
    // key は階層の値を辿った一意な文字列（開閉状態の保持に使う）
    expect(new Set(tree.flatMap((n) => [n.key, ...n.children.map((c) => c.key)])).size).toBe(3 + 4)
  })

  it('1 階層のパターンは平らな一覧。同じラベルの album はまとまる', () => {
    const p = parsePattern('%albumartist%')
    if ('error' in p) throw new Error(p.error)
    const tree = buildTree(albums, p)
    expect(tree.map((n) => [n.label, n.count, n.albumIds])).toEqual([
      ['A', 5, [1, 2]],
      ['B', 1, [3]],
      ['C', 5, [4, 5]],
    ])
    expect(tree.every((n) => n.children.length === 0)).toBe(true)
  })

  it('%folder% は rel_dir の階層をそのまま展開する（foobar の by folder structure）', () => {
    const p = parsePattern('%folder%')
    if ('error' in p) throw new Error(p.error)
    const tree = buildTree(
      [
        album({ id: 1, rel_dir: '東方Project/A-One/X', track_count: 2 }),
        album({ id: 2, rel_dir: '東方Project/A-One/Y', track_count: 1 }),
        album({ id: 3, rel_dir: 'J-Pop/花譜/Z', track_count: 3 }),
        album({ id: 4, rel_dir: 'Loose', track_count: 1 }),
      ],
      p,
    )
    expect(tree.map((n) => [n.label, n.count])).toEqual([
      ['J-Pop', 3],
      ['Loose', 1],
      ['東方Project', 3],
    ])
    expect(tree[2].children.map((n) => n.label)).toEqual(['A-One'])
    expect(tree[2].children[0].children.map((n) => [n.label, n.albumIds])).toEqual([
      ['X', [1]],
      ['Y', [2]],
    ])
    expect(tree[1].children).toEqual([])
    // Library 直下（rel_dir が空）のアルバムは「（なし）」の 1 ノードに入れる（消えない）
    const rootLevel = buildTree([album({ id: 5, rel_dir: '', track_count: 4 }), album({ id: 6, rel_dir: 'B', track_count: 1 })], p)
    expect(rootLevel.map((n) => [n.label, n.count, n.albumIds])).toEqual([
      ['B', 1, [6]],
      [EMPTY_LABEL, 4, [5]],
    ])
    // %folder% は他と混ぜられない
    expect(parsePattern('%folder%|%album%')).toMatchObject({ error: expect.stringContaining('folder') })
    expect(parsePattern('[%year%] %folder%')).toMatchObject({ error: expect.stringContaining('folder') })
  })

  it('並びは日本語 collator', () => {
    const p = parsePattern('%album%')
    if ('error' in p) throw new Error(p.error)
    const tree = buildTree(
      [album({ id: 1, album: 'い' }), album({ id: 2, album: 'あ' }), album({ id: 3, album: 'b' }), album({ id: 4, album: 'a' })],
      p,
    )
    expect(tree.map((n) => n.label)).toEqual(['a', 'b', 'あ', 'い'])
  })
})
