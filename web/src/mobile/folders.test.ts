import { describe, expect, it } from 'vitest'
import type { AlbumRow } from '../api/types'
import { buildTree, parsePattern, type Pattern } from '../lib/tree'
import { folderLevel } from './folders'

const a = (id: number, albumartist: string, album: string, category = 'J-Pop'): AlbumRow =>
  ({
    id,
    rel_dir: `${category}/${albumartist}/${album}`,
    category,
    albumartist,
    album,
    date: null,
    original_date: null,
    edition: null,
    mb_release_id: null,
    disc_count: null,
    artwork_id: null,
    artwork_hash: null,
    track_count: 10,
    duration_ms: 0,
    missing_since: null,
    album_gain: false,
  }) as AlbumRow

const tree = buildTree(
  [a(1, 'YOASOBI', 'THE BOOK'), a(2, 'YOASOBI', 'E-SIDE'), a(3, 'Vaundy', 'strobo')],
  parsePattern('%category%|%albumartist%|%album%') as Pattern,
)

describe('folderLevel', () => {
  it('段を降りる', () => {
    expect(folderLevel(tree, [])!.nodes.map((n) => n.label)).toEqual(['J-Pop'])
    expect(folderLevel(tree, ['J-Pop'])!.nodes.map((n) => n.label)).toEqual(['Vaundy', 'YOASOBI'])
    expect(folderLevel(tree, ['J-Pop', 'YOASOBI', 'THE BOOK'])).toEqual({ nodes: [], albumIds: [1] })
  })
  it('消えた段は null（呼び出し側が最上段へ戻す）', () => {
    expect(folderLevel(tree, ['Game'])).toBeNull()
    expect(folderLevel(tree, ['J-Pop', 'YOASOBI', 'nope'])).toBeNull()
  })
  it('同じ名前のアルバムは 1 ノードにまとまり、album id を全部返す', () => {
    const t = buildTree(
      [a(5, 'YOASOBI', 'THE BOOK'), a(4, 'YOASOBI', 'THE BOOK')],
      parsePattern('%category%|%albumartist%|%album%') as Pattern,
    )
    expect(folderLevel(t, ['J-Pop', 'YOASOBI', 'THE BOOK'])).toEqual({ nodes: [], albumIds: [4, 5] })
  })
})
