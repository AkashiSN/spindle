// フォルダのタブの純粋ロジック。カテゴリ → アルバムアーティスト → アルバム の木を path で降りる

import { parsePattern, type Pattern, type TreeNode } from '../lib/tree'

/** フォルダの段。lib/tree.ts の 'by category' と同じ */
export const FOLDER_PATTERN = parsePattern('%category%|%albumartist%|%album%') as Pattern

/** album の段（葉）の深さ */
export const FOLDER_DEPTH = 3

/**
 * path の段まで降りた先。葉（長さ 3）に達したら nodes は空・albumIds はその album。
 * 途中で見つからなければ null（呼び出し側が最上段へ戻す）
 */
export function folderLevel(
  tree: TreeNode[],
  path: string[],
): { nodes: TreeNode[]; albumIds: number[] | null } | null {
  let nodes = tree
  let node: TreeNode | undefined
  for (const label of path) {
    node = nodes.find((n) => n.label === label)
    if (!node) return null
    nodes = node.children
  }
  if (path.length >= FOLDER_DEPTH && node) return { nodes: [], albumIds: node.albumIds }
  return { nodes, albumIds: null }
}
