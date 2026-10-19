// プレイリストの名前を入れるシート（新規作成と名前の変更で共用）。
// 新規のときだけ「普通 / スマート」を選ばせる。スマートの作成は Task 9（条件の組み立て）で入る

import { useState, type FormEvent } from 'react'
import type { Playlist } from '../../api/types'
import { BottomSheet } from './BottomSheet'

export type NameSheetResult = { name: string; kind: Playlist['kind'] }

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
  /** 失敗の文言か null（成功。閉じるのは呼び出し側） */
  onSubmit: (r: NameSheetResult) => Promise<string | null>
  onClose: () => void
}) {
  const [name, setName] = useState(initial)
  const [kind, setKind] = useState<Playlist['kind']>('manual')
  const [error, setError] = useState<string | null>(null)
  const [saving, setSaving] = useState(false)

  const submit = (e: FormEvent) => {
    e.preventDefault()
    if (saving) return
    const trimmed = name.trim()
    if (trimmed === '') {
      setError('名前を入れてください')
      return
    }
    setSaving(true)
    setError(null)
    onSubmit({ name: trimmed, kind })
      .then((err) => {
        if (err != null) setError(err)
      })
      .finally(() => setSaving(false))
  }

  return (
    <BottomSheet label={label} expanded={false} onClose={onClose} className="m-tagedit">
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
              {/* Task 9: スマートを選べるようにし、作成画面で最初の条件を 1 つ組ませてから createPlaylist({ name, rule }) */}
              <button type="button" role="radio" aria-checked={false} disabled>
                ✦ スマート
              </button>
            </div>
            <p className="m-note">スマートプレイリストの作成はまだスマホ版ではできません（PC で作成してください）</p>
          </>
        )}
        {error != null && <p className="m-error">{error}</p>}
        <div className="m-tagedit-actions">
          <button type="button" onClick={onClose} disabled={saving}>
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
