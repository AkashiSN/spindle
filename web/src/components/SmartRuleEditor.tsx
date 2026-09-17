// スマートプレイリストのルール編集（P1-7、docs/DSL.md、D-54）。中央ペインの表の上に出す。
// 入力から 250ms 後にサーバで検証・件数を取り、表は `filter.dsl` で WHERE の結果を追随する
// （ORDER BY / LIMIT は保存時の評価にだけ効く）

import { useEffect, useRef, useState } from 'react'
import { ApiError } from '../api/client'
import type { Playlists } from '../hooks/usePlaylists'

export type RuleDraft = {
  /** 編集中のプレイリスト。null なら新規 */
  id: number | null
  name: string
  rule: string
}

const HELP =
  '%albumartist% IS ヰ世界情緒 AND NOT %category% IS _Unsorted ORDER BY %date% DESC LIMIT 100\n' +
  '演算子: IS / HAS / GREATER / LESS / MATCHES / PRESENT / MISSING、論理: AND / OR / NOT / ()\n' +
  'フィールド: 任意のタグ名と category verification source_type lossless codec samplerate bitdepth channels bitrate duration added has_derived missing'

export function SmartRuleEditor({
  draft,
  playlists,
  onDraft,
  onSaved,
  onCancel,
}: {
  draft: RuleDraft
  playlists: Playlists
  /** テキストが変わるたび（表の dsl フィルタに使う） */
  onDraft: (d: RuleDraft) => void
  onSaved: (id: number) => void
  onCancel: () => void
}) {
  const [status, setStatus] = useState<{ kind: 'idle' | 'ok' | 'error'; text: string }>({ kind: 'idle', text: '' })
  const [saving, setSaving] = useState(false)
  const timer = useRef<number | null>(null)
  const abort = useRef<AbortController | null>(null)
  const { previewRule } = playlists
  const shown = draft.rule.trim() ? status : { kind: 'idle' as const, text: '' }

  // 250ms デバウンスで検証（表のフィルタも同じタイミングで onDraft 済み）
  useEffect(() => {
    const rule = draft.rule.trim()
    if (timer.current != null) window.clearTimeout(timer.current)
    abort.current?.abort()
    // 空は検証しない（描画時に idle として扱う。effect 内で同期的に setState しない）
    if (!rule) return
    timer.current = window.setTimeout(() => {
      const ac = new AbortController()
      abort.current = ac
      previewRule(rule, ac.signal)
        .then((r) => setStatus({ kind: 'ok', text: `${r.count.toLocaleString('ja-JP')} 件` }))
        .catch((e: unknown) => {
          if (ac.signal.aborted) return
          setStatus({ kind: 'error', text: e instanceof Error ? e.message : String(e) })
        })
    }, 250)
    return () => {
      if (timer.current != null) window.clearTimeout(timer.current)
    }
  }, [draft.rule, previewRule])

  const save = async () => {
    const name = draft.name.trim()
    const rule = draft.rule.trim()
    if (!name || !rule) return
    setSaving(true)
    try {
      const p = draft.id == null ? await playlists.create(name, rule) : await playlists.setRule(draft.id, rule)
      onSaved(p.id)
    } catch (e) {
      const msg = e instanceof ApiError && e.code === 'duplicate' ? '同じ名前のプレイリストがある' : e instanceof Error ? e.message : String(e)
      setStatus({ kind: 'error', text: `保存に失敗: ${msg}` })
    } finally {
      setSaving(false)
    }
  }

  return (
    <div className="rule-editor">
      <div className="rule-head">
        <span className="muted">⚙ スマートプレイリスト</span>
        {draft.id == null ? (
          <input
            className="rule-name"
            placeholder="名前"
            value={draft.name}
            onChange={(e) => onDraft({ ...draft, name: e.target.value })}
          />
        ) : (
          <strong>{draft.name}</strong>
        )}
        <span className="spacer" />
        <span className={`rule-status ${shown.kind}`}>{shown.text}</span>
        <button type="button" onClick={() => void save()} disabled={saving || shown.kind !== 'ok' || !draft.name.trim()}>
          {draft.id == null ? '作成' : '保存して再評価'}
        </button>
        <button type="button" className="ghost" onClick={onCancel}>
          キャンセル
        </button>
      </div>
      <textarea
        className="rule-text"
        rows={3}
        spellCheck={false}
        placeholder={HELP}
        title={HELP}
        value={draft.rule}
        onChange={(e) => {
          // 検証結果が出るまで保存できない（古い件数で保存しない）
          setStatus({ kind: 'idle', text: '検証中…' })
          onDraft({ ...draft, rule: e.target.value })
        }}
        onKeyDown={(e) => {
          if (e.key === 'Escape') onCancel()
          // Ctrl+Enter で保存
          if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) void save()
        }}
      />
      <p className="muted small">表は WHERE の結果（ORDER BY / LIMIT は保存時の評価にだけ効く）。Ctrl+Enter で保存、Esc で閉じる</p>
    </div>
  )
}
