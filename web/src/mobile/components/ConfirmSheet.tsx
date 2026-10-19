// 確認のシート（削除など取り返しにくい操作の前に挟む。スマホは誤タップしやすいため）。
// onConfirm が文言を返したら失敗としてシートに出し、null なら閉じるのは呼び出し側

import { useState } from 'react'
import { BottomSheet } from './BottomSheet'

export function ConfirmSheet({
  label,
  message,
  confirmLabel,
  danger,
  onConfirm,
  onClose,
}: {
  label: string
  message: string
  confirmLabel: string
  danger?: boolean
  /** 失敗の文言か null（成功） */
  onConfirm: () => Promise<string | null>
  onClose: () => void
}) {
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const confirm = () => {
    if (busy) return
    setBusy(true)
    setError(null)
    onConfirm()
      .then((err) => {
        if (err != null) setError(err)
      })
      .finally(() => setBusy(false))
  }
  return (
    <BottomSheet label={label} expanded={false} onClose={onClose} className="m-tagedit">
      <div className="m-tagedit-form">
        <p className="m-confirm-msg">{message}</p>
        {error != null && <p className="m-error">{error}</p>}
        <div className="m-tagedit-actions">
          <button type="button" onClick={onClose} disabled={busy}>
            やめる
          </button>
          <button type="button" className={danger ? 'm-danger' : 'm-primary'} onClick={confirm} disabled={busy}>
            {busy ? '処理中…' : confirmLabel}
          </button>
        </div>
      </div>
    </BottomSheet>
  )
}
