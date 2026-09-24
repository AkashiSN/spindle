// 右パネル「一括編集」タブ（SPEC §12.3、D-87）: 横に並ぶ番号付きの段 ① 対象 → ② 操作 → ③ プレビュー → ④ 適用。
// パネルは表の上で低く横長なので段は横に並べ、狭ければ 2 列 → 1 列にする（コンテナ幅）。
// プレビュー結果の差分は表のセルに重ねる（TrackTable）。ここは操作リストと件数・適用の流れだけ

import { useState } from 'react'
import type { BatchEdit } from '../hooks/useBatchEdit'
import { formatCount } from '../lib/format'
import { COMMON_KEYS, moveOp, newOp, OP_KINDS, OP_LABELS, type OpKind, type TagOp } from '../lib/tagops'
import { PanelTargetStep, type PanelTarget } from './PanelTargetStep'
import { Step } from './Step'

export function BatchEditPanel({
  edit,
  hasSelection,
  target,
}: {
  edit: BatchEdit
  hasSelection: boolean
  target: PanelTarget
}) {
  const [description, setDescription] = useState('')
  const [addKind, setAddKind] = useState<OpKind>('set')
  const { ops, setOps, preview, stale, status, error, pendingPrompt } = edit
  const busy = status !== 'idle'
  const fresh = preview != null
  const applied = edit.lastBatchId != null && !fresh && !stale && error == null

  const update = (id: string, patch: Partial<TagOp>) =>
    setOps(ops.map((o) => (o.id === id ? ({ ...o, ...patch } as TagOp) : o)))
  const remove = (id: string) => setOps(ops.filter((o) => o.id !== id))

  const onApply = async (skipPending = false) => {
    const r = await edit.apply(description, skipPending)
    if (r) setDescription('')
  }

  return (
    <div className="batch-edit panel-steps">
      <datalist id="tag-keys">
        {COMMON_KEYS.map((k) => (
          <option key={k} value={k} />
        ))}
      </datalist>
      <PanelTargetStep target={target} hasSelection={hasSelection} />
      <Step no={2} title="操作" aside="上から順に適用" done={ops.length > 0}>
        <ol className="op-list">
          {ops.map((o, i) => (
            <li key={o.id} className="op">
              <div className="op-head">
                <span className="op-index">{i + 1}.</span>
                <span className="op-kind">{OP_LABELS[o.op]}</span>
                <span className="spacer" />
                <button type="button" className="ghost" title="上へ" disabled={i === 0} onClick={() => setOps(moveOp(ops, o.id, -1))}>
                  ↑
                </button>
                <button
                  type="button"
                  className="ghost"
                  title="下へ"
                  disabled={i === ops.length - 1}
                  onClick={() => setOps(moveOp(ops, o.id, 1))}
                >
                  ↓
                </button>
                <button type="button" className="ghost" title="削除" onClick={() => remove(o.id)}>
                  ×
                </button>
              </div>
              <div className="op-fields">
                <input
                  list="tag-keys"
                  placeholder="キー（TITLE など）"
                  value={o.key}
                  onChange={(e) => update(o.id, { key: e.target.value })}
                  aria-label="キー"
                />
                {o.op === 'set' && (
                  <input
                    placeholder="値（空なら削除）"
                    value={o.value}
                    onChange={(e) => update(o.id, { value: e.target.value })}
                    aria-label="値"
                  />
                )}
                {o.op === 'ref' && (
                  <input
                    placeholder="%artist% のようにフィールドを参照"
                    value={o.template}
                    onChange={(e) => update(o.id, { template: e.target.value })}
                    aria-label="テンプレート"
                  />
                )}
                {o.op === 'replace' && (
                  <>
                    <input
                      placeholder="パターン（正規表現）"
                      value={o.pattern}
                      onChange={(e) => update(o.id, { pattern: e.target.value })}
                      aria-label="パターン"
                    />
                    <input
                      placeholder="置換（$1 で参照）"
                      value={o.replacement}
                      onChange={(e) => update(o.id, { replacement: e.target.value })}
                      aria-label="置換"
                    />
                  </>
                )}
                {o.op === 'number' && (
                  <div className="op-inline">
                    <label>
                      開始
                      <input
                        type="number"
                        min={0}
                        value={o.start}
                        onChange={(e) => update(o.id, { start: Number(e.target.value) })}
                      />
                    </label>
                    <label>
                      桁
                      <input
                        type="number"
                        min={0}
                        max={6}
                        value={o.pad}
                        onChange={(e) => update(o.id, { pad: Number(e.target.value) })}
                      />
                    </label>
                    <span className="muted small">現在のソート順に</span>
                  </div>
                )}
              </div>
            </li>
          ))}
        </ol>
        <div className="op-add">
          <select value={addKind} onChange={(e) => setAddKind(e.target.value as OpKind)} aria-label="追加する操作">
            {OP_KINDS.map((k) => (
              <option key={k} value={k}>
                {OP_LABELS[k]}
              </option>
            ))}
          </select>
          <button type="button" onClick={() => setOps([...ops, newOp(addKind)])}>
            + 操作を追加
          </button>
        </div>
      </Step>

      <Step
        no={3}
        title="プレビュー"
        aside={stale ? <span className="badge warn">古い</span> : undefined}
        wait={!hasSelection || ops.length === 0}
        done={fresh}
        stale={stale}
        hint="差分は下の表のセルに重ねて出す（旧 → 新）。変更なしの曲はバッチに入らない"
      >
        <div className="op-actions">
          {preview && (
            <div className="preview-counts panel-counts">
              <div className="changed">
                <span className="muted small">変更</span>
                <b>{formatCount(preview.counts.changed)}</b>
              </div>
              <div>
                <span className="muted small">変更なし</span>
                <b>{formatCount(preview.counts.unchanged)}</b>
              </div>
              <div>
                <span className="muted small">反映待ち除外</span>
                <b>{formatCount(preview.counts.pending_excluded)}</b>
              </div>
            </div>
          )}
          {stale && <div className="panel-banner warn small">選択・操作・ソートを変えた。プレビューし直す</div>}
          {fresh && <div className="muted small">差分は下の表に重ねて表示</div>}
          <button
            type="button"
            className={fresh ? undefined : 'primary'}
            disabled={busy || !hasSelection || ops.length === 0}
            onClick={() => void edit.runPreview()}
          >
            {status === 'previewing' ? 'プレビュー中…' : fresh || stale ? 'プレビューし直す' : 'プレビュー'}
          </button>
        </div>
      </Step>

      <Step
        no={4}
        title="適用"
        aside={<span className="badge undo">巻き戻せる</span>}
        wait={!fresh && !applied}
        done={applied}
      >
        <div className="op-actions">
          {applied ? (
            <div className="panel-banner ok small">
              バッチ #{edit.lastBatchId} を作成。反映は下部バーで進む。戻すときは履歴から巻き戻す
            </div>
          ) : (
            <>
              <input
                placeholder="説明（任意。履歴に残る）"
                value={description}
                disabled={!fresh}
                onChange={(e) => setDescription(e.target.value)}
                aria-label="説明"
              />
              <button
                type="button"
                className="primary"
                disabled={busy || !preview || preview.counts.changed === 0}
                onClick={() => void onApply(false)}
              >
                {status === 'applying'
                  ? '適用中…'
                  : preview
                    ? `${formatCount(preview.counts.changed)} 曲に適用`
                    : '適用'}
              </button>
              {!fresh && <span className="muted small">③ のプレビューが済むと押せる</span>}
              {preview && preview.counts.changed === 0 && <span className="muted small">変わる曲が無い</span>}
            </>
          )}
          {pendingPrompt && (
            <div className="pending-prompt" role="alertdialog">
              <p>
                対象のうち {formatCount(pendingPrompt.count)} 件が反映待ちです。
              </p>
              <button type="button" onClick={() => void onApply(true)}>
                {formatCount(pendingPrompt.count)} 件を除外して適用
              </button>{' '}
              <button type="button" className="ghost" onClick={edit.dismissPending}>
                待つ
              </button>
            </div>
          )}
          {error && <div className="error small">{error}</div>}
        </div>
      </Step>
    </div>
  )
}
