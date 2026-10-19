// 1 曲・1 フィールドのタグの修正。保存は PC と同じ preview → apply（edit_batches に載る）

import { useState, type FormEvent } from 'react'
import { editTag } from '../api'
import { tagEditProblem } from '../track'
import { BottomSheet } from './BottomSheet'

export function TagEditSheet({
  trackId,
  tagKey,
  label,
  initial,
  onClose,
  onSaved,
}: {
  trackId: number
  tagKey: string
  label: string
  initial: string
  onClose: () => void
  onSaved: () => void
}) {
  const [text, setText] = useState(initial)
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  const submit = (e: FormEvent) => {
    e.preventDefault()
    if (saving) return
    const problem = tagEditProblem(text)
    if (problem) {
      setError(problem)
      return
    }
    setSaving(true)
    setError(null)
    editTag(trackId, tagKey, text)
      .then((err) => {
        if (err == null) onSaved()
        else setError(err)
      })
      .finally(() => setSaving(false))
  }

  return (
    <BottomSheet label={`${label} を修正`} expanded={false} onClose={onClose} className="m-tagedit">
      <form className="m-tagedit-form" onSubmit={submit}>
        <label>
          <span className="m-tagedit-key">{label}</span>
          <input
            type="text"
            value={text}
            autoFocus
            autoCapitalize="off"
            autoCorrect="off"
            spellCheck={false}
            enterKeyHint="done"
            onChange={(e) => setText(e.target.value)}
          />
        </label>
        <p className="m-note">複数の値は「;」で区切ります。</p>
        <p className="m-note">この曲 1 曲だけに書き込みます。PC の「履歴」から巻き戻せます</p>
        {error != null && <p className="m-error">{error}</p>}
        <div className="m-tagedit-actions">
          <button type="button" onClick={onClose} disabled={saving}>
            やめる
          </button>
          <button type="submit" className="m-primary" disabled={saving}>
            {saving ? '保存中…' : '保存'}
          </button>
        </div>
      </form>
    </BottomSheet>
  )
}
