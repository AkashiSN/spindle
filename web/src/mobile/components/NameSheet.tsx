// プレイリストの名前を入れるシート（新規作成と名前の変更で共用）。
// 新規のときだけ「普通 / スマート」を選ばせる。スマートは空のルールで作れないので、最初の条件を 1 つ組ませて
// rule に入れる（続きは作った後の「条件を編集」で）。戻るで閉じる（useBackClose）

import { useState, type FormEvent } from 'react'
import type { Playlist } from '../../api/types'
import { useBackClose } from '../backClose'
import { dslOrError, NEW_CONDITION, type Condition } from '../ruleBuilder'
import { useRulePreview } from '../useRulePreview'
import { BottomSheet } from './BottomSheet'
import { ConditionRow, PreviewStatus } from './SmartEditor'

/** スマートなら rule（DSL）がある */
export type NameSheetResult = { name: string; kind: Playlist['kind']; rule?: string }

export function NameSheet({
  label,
  initial,
  chooseKind,
  submitLabel,
  onSubmit,
  onClose,
}: {
  label: string
  initial: string
  /** 新規作成なら true（種類を選ばせる） */
  chooseKind: boolean
  submitLabel: string
  /** 失敗の文言、null（成功。シートを閉じる）、または関数（シートを閉じた後に呼ぶ。画面を移るとき） */
  onSubmit: (r: NameSheetResult) => Promise<string | null | (() => void)>
  /** シートが閉じた（戻る・やめる・成功） */
  onClose: () => void
}) {
  const close = useBackClose(onClose)
  const [name, setName] = useState(initial)
  const [kind, setKind] = useState<Playlist['kind']>('manual')
  const [cond, setCond] = useState<Condition>(NEW_CONDITION)
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)
  const smart = chooseKind && kind === 'smart'
  const built = smart ? dslOrError({ join: 'and', conditions: [cond], order: null, limit: null }) : null
  const preview = useRulePreview(built != null && 'dsl' in built ? built.dsl : null)

  const submit = (e: FormEvent) => {
    e.preventDefault()
    if (saving) return
    const trimmed = name.trim()
    if (trimmed === '') {
      setError('名前を入れてください')
      return
    }
    let rule: string | undefined
    if (built != null) {
      if ('error' in built) {
        setError(built.error)
        return
      }
      if (preview.kind !== 'ok') {
        setError(preview.kind === 'error' ? preview.message : '条件を確かめています。少し待ってから押してください')
        return
      }
      rule = built.dsl
    }
    setSaving(true)
    setError(null)
    onSubmit(rule != null ? { name: trimmed, kind, rule } : { name: trimmed, kind })
      .then((r) => {
        if (typeof r === 'string') setError(r)
        else close(r ?? undefined)
      })
      .finally(() => setSaving(false))
  }

  return (
    <BottomSheet label={label} expanded={false} onClose={() => close()} className="m-tagedit">
      <form className="m-tagedit-form" onSubmit={submit}>
        <label>
          <span className="m-tagedit-key">名前</span>
          <input
            type="text"
            value={name}
            autoFocus
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            enterKeyHint="done"
            onChange={(e) => setName(e.target.value)}
          />
        </label>
        {chooseKind && (
          <>
            <div className="m-segmented m-kind" role="radiogroup" aria-label="種類">
              <button
                type="button"
                role="radio"
                aria-checked={kind === 'manual'}
                className={kind === 'manual' ? 'active' : undefined}
                onClick={() => setKind('manual')}
              >
                ♪ 普通
              </button>
              <button
                type="button"
                role="radio"
                aria-checked={kind === 'smart'}
                className={kind === 'smart' ? 'active' : undefined}
                onClick={() => setKind('smart')}
              >
                ✦ スマート
              </button>
            </div>
            {smart && (
              <>
                <span className="m-tagedit-key">最初の条件（続きは作った後に「条件を編集」で足せます）</span>
                <ConditionRow c={cond} onChange={setCond} />
                <PreviewStatus state={preview} error={built != null && 'error' in built ? built.error : null} />
              </>
            )}
          </>
        )}
        {error != null && <p className="m-error">{error}</p>}
        <div className="m-tagedit-actions">
          <button type="button" onClick={() => close()} disabled={saving}>
            やめる
          </button>
          <button type="submit" className="m-primary" disabled={saving}>
            {saving ? '保存中…' : submitLabel}
          </button>
        </div>
      </form>
    </BottomSheet>
  )
}
