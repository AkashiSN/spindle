// 右パネル「操作」タブ（D-58）: API だけあって UI が無かった機能の起動導線。
// リネーム / 正規化は preview（old → new と衝突理由の一覧）→ 適用、RG 解析 / RG 書き込み / FLAC 検査は
// 投入して件数を出す。プレイリストへ追加もここ。状態は hooks/useOperations

import { useState } from 'react'
import type { Playlist } from '../api/types'
import type { Operations, PathKind } from '../hooks/useOperations'
import { formatCount } from '../lib/format'
import { pathPreviewSummary } from '../lib/operations'

const PATH_LABEL: Record<PathKind, string> = { rename: 'リネーム', normalize: '正規化（→ FLAC）' }

export function OperationsPanel({
  ops,
  hasSelection,
  playlists,
  onAddToPlaylist,
}: {
  ops: Operations
  hasSelection: boolean
  /** 「プレイリストへ追加」の候補（手動のみ）と追加の実行（P1-6） */
  playlists: Playlist[]
  onAddToPlaylist: (playlistId: number) => void
}) {
  const [description, setDescription] = useState('')
  const [addTo, setAddTo] = useState<number | ''>('')
  const busy = ops.busy != null
  const pv = ops.pathPreview
  const manual = playlists.filter((p) => p.kind === 'manual')

  const label = (id: string, text: string) => (ops.busy === id ? `${text}…` : text)

  return (
    <div className="operations">
      <section>
        <h4>ファイル</h4>
        <div className="op-row">
          <button type="button" disabled={busy || !hasSelection} onClick={() => void ops.previewPaths('rename')}>
            {label('preview:rename', 'リネームをプレビュー')}
          </button>
          <button type="button" disabled={busy || !hasSelection} onClick={() => void ops.previewPaths('normalize')}>
            {label('preview:normalize', '正規化をプレビュー')}
          </button>
          <span className="muted small">[layout] の規則で並べ直す / WAV・ALAC を FLAC にする</span>
        </div>
        {pv && (
          <div className="path-preview">
            <div className="preview-counts">
              {PATH_LABEL[pv.kind]}: {pathPreviewSummary(pv.preview)}（対象 {formatCount(pv.preview.count)} 件）
            </div>
            {pv.preview.items.length > 0 && (
              <table className="path-items">
                <tbody>
                  {pv.preview.items.map((it) => (
                    <tr key={it.id} className={it.new == null ? 'conflict' : ''}>
                      <td className="old">{it.old}</td>
                      <td className="arrow">→</td>
                      <td className="new">{it.new ?? <span className="reason">{it.reason ?? '生成できない'}</span>}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
            )}
            <div className="op-row">
              <input
                placeholder="説明（任意。履歴に残る）"
                value={description}
                onChange={(e) => setDescription(e.target.value)}
                aria-label="説明"
              />
              <button
                type="button"
                className="primary"
                disabled={busy || pv.preview.changed === 0}
                onClick={() => {
                  void ops.applyPaths(description).then((ok) => {
                    if (ok) setDescription('')
                  })
                }}
              >
                {label(`apply:${pv.kind}`, `${PATH_LABEL[pv.kind]}を適用`)}
              </button>
            </div>
          </div>
        )}
        {!pv && hasSelection && <div className="muted small">選択・ソートを変えたらプレビューし直す</div>}
      </section>

      <section>
        <h4>ReplayGain / FLAC</h4>
        <div className="op-row">
          <button type="button" disabled={busy || !hasSelection} onClick={() => void ops.startRg()}>
            {label('rg', 'ReplayGain を解析')}
          </button>
          <button type="button" disabled={busy || !hasSelection} onClick={() => void ops.writeRg()}>
            {label('rgwrite', '解析値をタグに書く')}
          </button>
          <button type="button" disabled={busy || !hasSelection} onClick={() => void ops.startFlaccheck()}>
            {label('flaccheck', 'FLAC を検査')}
          </button>
          <span className="muted small">解析はアルバム単位のジョブ。書き込みは巻き戻せるバッチ</span>
        </div>
      </section>

      <section>
        <h4>プレイリスト</h4>
        <div className="op-row add-to-playlist">
          <select
            value={addTo}
            disabled={!hasSelection || manual.length === 0}
            onChange={(e) => setAddTo(e.target.value === '' ? '' : Number(e.target.value))}
            aria-label="追加先のプレイリスト"
          >
            <option value="">{manual.length === 0 ? '手動プレイリストが無い' : 'プレイリストへ追加…'}</option>
            {manual.map((p) => (
              <option key={p.id} value={p.id}>
                {p.name}
              </option>
            ))}
          </select>
          <button
            type="button"
            disabled={!hasSelection || addTo === ''}
            onClick={() => {
              if (addTo !== '') onAddToPlaylist(addTo)
            }}
          >
            追加
          </button>
        </div>
      </section>

      {ops.pendingPrompt && (
        <div className="pending-prompt" role="alertdialog">
          <p>対象のうち {formatCount(ops.pendingPrompt.count)} 件が反映待ちです。</p>
          <button
            type="button"
            onClick={() => {
              // pendingPrompt は現在の選択に紐づくもの（hooks/useOperations）だけが渡ってくる
              const p = ops.pendingPrompt
              if (!p) return
              if (p.action === 'paths') {
                void ops.applyPaths(description, true).then((ok) => {
                  if (ok) setDescription('')
                })
              } else {
                void ops.writeRg(true)
              }
            }}
          >
            {formatCount(ops.pendingPrompt.count)} 件を除外して適用
          </button>{' '}
          <button type="button" className="ghost" onClick={ops.dismissPending}>
            待つ
          </button>
        </div>
      )}
      {ops.error && <p className="error small">{ops.error}</p>}
      {ops.notice && (
        <p className="notice small">
          {ops.notice}{' '}
          <button type="button" className="ghost" onClick={ops.clearNotice} title="閉じる">
            ×
          </button>
        </p>
      )}
    </div>
  )
}
