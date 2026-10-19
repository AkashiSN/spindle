// スマートプレイリストの組み立て画面。「次の条件を すべて / いずれか 満たす曲」と条件の行
// （フィールド / NOT / 演算子 / 値 / ×）、並び順、件数の上限。入力のたびに DSL にして 250ms 後に件数を確かめる。
// 値に " がある・フィールド名が不正なときは DSL にできないので、保存させずに文言を出す

import { useState } from 'react'
import type { BuilderModel, CmpOp, Condition } from '../ruleBuilder'
import { dslOrError, FIELD_CHOICES, NEW_CONDITION } from '../ruleBuilder'
import type { RulePreviewState } from '../useRulePreview'

type OpChoice = CmpOp | 'present' | 'missing'

const OP_LABELS: readonly [OpChoice, string][] = [
  ['is', 'IS（一致）'],
  ['has', 'HAS（含む）'],
  ['greater', 'GREATER（より大きい）'],
  ['less', 'LESS（より小さい）'],
  ['matches', 'MATCHES（正規表現）'],
  ['present', 'PRESENT（タグがある）'],
  ['missing', 'MISSING（タグが無い）'],
]

const OTHER = '\u0000other'

/** 件数の表示（✓ 86 件 / 誤りの文言） */
export function PreviewStatus({ state, error }: { state: RulePreviewState; error?: string | null }) {
  if (error != null) return <p className="m-error m-rule-status">{error}</p>
  switch (state.kind) {
    case 'idle':
      return null
    case 'loading':
      return <p className="m-note m-rule-status">確かめ中…</p>
    case 'ok':
      return <p className="m-rule-status m-rule-ok">✓ {state.count.toLocaleString('ja-JP')} 件</p>
    case 'error':
      return <p className="m-error m-rule-status">{state.message}</p>
  }
}

/** 条件の 1 行。onRemove が無ければ × を出さない（条件が 1 つだけのとき） */
export function ConditionRow({
  c,
  onChange,
  onRemove,
}: {
  c: Condition
  onChange: (c: Condition) => void
  onRemove?: () => void
}) {
  // 「その他のタグ…」を選んだら、入力が候補と同じ名前になっても自由入力のまま（選び直すまで）
  const [custom, setCustom] = useState(() => !FIELD_CHOICES.includes(c.field))
  const known = !custom && FIELD_CHOICES.includes(c.field)
  const op: OpChoice = c.kind === 'cmp' ? c.cmp : c.kind
  const setOp = (v: OpChoice) => {
    if (v === 'present' || v === 'missing') onChange({ kind: v, field: c.field })
    else if (c.kind === 'cmp') onChange({ ...c, cmp: v })
    else onChange({ kind: 'cmp', not: false, field: c.field, cmp: v, value: '' })
  }
  return (
    <div className="m-cond">
      <div className="m-cond-line">
        <select
          aria-label="フィールド"
          value={known ? c.field : OTHER}
          onChange={(e) => {
            const other = e.target.value === OTHER
            setCustom(other)
            onChange({ ...c, field: other ? '' : e.target.value })
          }}
        >
          {FIELD_CHOICES.map((f) => (
            <option key={f} value={f}>
              {f}
            </option>
          ))}
          <option value={OTHER}>その他のタグ…</option>
        </select>
        {!known && (
          <input
            type="text"
            aria-label="タグ名"
            placeholder="タグ名（英数字と _）"
            value={c.field}
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            onChange={(e) => onChange({ ...c, field: e.target.value })}
          />
        )}
        {onRemove && (
          <button type="button" className="m-icon m-cond-remove" aria-label="この条件を消す" onClick={onRemove}>
            ×
          </button>
        )}
      </div>
      <div className="m-cond-line">
        {c.kind === 'cmp' && (
          <button
            type="button"
            className={c.not ? 'm-cond-not active' : 'm-cond-not'}
            aria-pressed={c.not}
            onClick={() => onChange({ ...c, not: !c.not })}
          >
            NOT
          </button>
        )}
        <select aria-label="演算子" value={op} onChange={(e) => setOp(e.target.value as OpChoice)}>
          {OP_LABELS.map(([v, label]) => (
            <option key={v} value={v}>
              {label}
            </option>
          ))}
        </select>
        {c.kind === 'cmp' && (
          <input
            type="text"
            aria-label="値"
            placeholder="値"
            value={c.value}
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            inputMode={c.cmp === 'greater' || c.cmp === 'less' ? 'decimal' : undefined}
            onChange={(e) => onChange({ ...c, value: e.target.value })}
          />
        )}
      </div>
    </div>
  )
}

/** 並び順の選択値（'' はなし、'\u0000random' はランダム、それ以外はフィールド名） */
const RANDOM = '\u0000random'
const LIMIT_ERROR = '件数の上限は 0 以上の整数で入れてください'

export function SmartEditor({
  initial,
  preview,
  onDraft,
}: {
  initial: BuilderModel
  /** 今の文（onDraft で渡したもの）の件数 */
  preview: RulePreviewState
  /** 条件が変わるたび。DSL にできればその文、できなければ文言 */
  onDraft: (r: { dsl: string } | { error: string }) => void
}) {
  const [m, setM] = useState<BuilderModel>(initial)
  const [limitText, setLimitText] = useState(initial.limit == null ? '' : String(initial.limit))
  const [limitError, setLimitError] = useState(false)
  const result = dslOrError(m)

  const update = (next: BuilderModel, badLimit: boolean = limitError) => {
    setM(next)
    onDraft(badLimit ? { error: LIMIT_ERROR } : dslOrError(next))
  }
  // 行の key（消しても他の行の入力状態がずれないように、位置ではなく採番した値）
  const [keys, setKeys] = useState(() => initial.conditions.map((_, i) => i))
  const [nextKey, setNextKey] = useState(initial.conditions.length)
  const setCond = (i: number, c: Condition) => update({ ...m, conditions: m.conditions.map((x, j) => (j === i ? c : x)) })
  const removeCond = (i: number) => {
    setKeys((ks) => ks.filter((_, j) => j !== i))
    update({ ...m, conditions: m.conditions.filter((_, j) => j !== i) })
  }
  const addCond = () => {
    setKeys((ks) => [...ks, nextKey])
    setNextKey((k) => k + 1)
    update({ ...m, conditions: [...m.conditions, { ...NEW_CONDITION }] })
  }

  const orderValue = m.order == null ? '' : 'desc' in m.order ? m.order.field : RANDOM
  const orderFields = m.order != null && 'desc' in m.order && !FIELD_CHOICES.includes(m.order.field)
    ? [m.order.field, ...FIELD_CHOICES]
    : FIELD_CHOICES
  const setOrder = (v: string) => {
    if (v === '') update({ ...m, order: null })
    else if (v === RANDOM) update({ ...m, order: { field: 'random' } })
    else update({ ...m, order: { field: v, desc: m.order != null && 'desc' in m.order ? m.order.desc : false } })
  }
  const setLimit = (text: string) => {
    setLimitText(text)
    const t = text.trim()
    if (t === '') {
      setLimitError(false)
      update({ ...m, limit: null }, false)
    } else if (/^[0-9]+$/.test(t) && Number.isSafeInteger(Number(t))) {
      setLimitError(false)
      update({ ...m, limit: Number(t) }, false)
    } else {
      setLimitError(true)
      update(m, true)
    }
  }

  return (
    <div className="m-builder">
      <div className="m-builder-join">
        <span>次の条件を</span>
        <div className="m-segmented m-join" role="radiogroup" aria-label="条件のつなぎ方">
          {(
            [
              ['and', 'すべて'],
              ['or', 'いずれか'],
            ] as const
          ).map(([v, label]) => (
            <button
              key={v}
              type="button"
              role="radio"
              aria-checked={m.join === v}
              className={m.join === v ? 'active' : undefined}
              onClick={() => update({ ...m, join: v })}
            >
              {label}
            </button>
          ))}
        </div>
        <span>満たす曲</span>
      </div>

      {m.conditions.map((c, i) => (
        <ConditionRow
          key={keys[i] ?? `x${i}`}
          c={c}
          onChange={(x) => setCond(i, x)}
          onRemove={m.conditions.length > 1 ? () => removeCond(i) : undefined}
        />
      ))}
      <button type="button" className="m-builder-add" onClick={addCond}>
        ＋ 条件を追加
      </button>

      <div className="m-builder-opt">
        <span className="m-tagedit-key">並び順</span>
        <div className="m-cond-line">
          <select aria-label="並び順" value={orderValue} onChange={(e) => setOrder(e.target.value)}>
            <option value="">なし</option>
            <option value={RANDOM}>ランダム</option>
            {orderFields.map((f) => (
              <option key={f} value={f}>
                {f}
              </option>
            ))}
          </select>
          {m.order != null && 'desc' in m.order && (
            <div className="m-segmented m-dir" role="radiogroup" aria-label="向き">
              {(
                [
                  [false, '昇順'],
                  [true, '降順'],
                ] as const
              ).map(([desc, label]) => (
                <button
                  key={label}
                  type="button"
                  role="radio"
                  aria-checked={m.order != null && 'desc' in m.order && m.order.desc === desc}
                  className={m.order != null && 'desc' in m.order && m.order.desc === desc ? 'active' : undefined}
                  onClick={() => m.order != null && 'desc' in m.order && update({ ...m, order: { ...m.order, desc } })}
                >
                  {label}
                </button>
              ))}
            </div>
          )}
        </div>
      </div>

      <label className="m-builder-opt">
        <span className="m-tagedit-key">件数の上限</span>
        <input
          type="text"
          inputMode="numeric"
          placeholder="なし"
          value={limitText}
          onChange={(e) => setLimit(e.target.value)}
        />
      </label>

      <PreviewStatus
        state={preview}
        error={limitError ? LIMIT_ERROR : 'error' in result ? result.error : null}
      />
    </div>
  )
}
