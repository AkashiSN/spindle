// 確認のシート（削除など取り返しにくい操作の前に挟む。スマホは誤タップしやすいため）。
// onConfirm が文言を返したら失敗としてシートに出し、null か関数なら閉じる（関数は閉じた後に呼ぶ）。戻るで閉じる

import { useState } from 'react'
import { useBackClose } from '../backClose'
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
  /** 失敗の文言、null（成功。閉じる）、または関数（閉じた後に呼ぶ。画面を移るとき） */
  onConfirm: () => Promise<string | null | (() => void)>
  /** シートが閉じた（戻る・やめる・成功） */
  onClose: () => void
}) {
  const close = useBackClose(onClose)
  const [busy, setBusy] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const confirm = () => {
    if (busy) return
    setBusy(true)
    setError(null)
    onConfirm()
      .then((r) => {
        if (typeof r === 'string') setError(r)
        else close(r ?? undefined)
      })
      .finally(() => setBusy(false))
  }
  return (
    <BottomSheet label={label} expanded={false} onClose={() => close()} className="m-tagedit">
      <div className="m-tagedit-form">
        <p className="m-confirm-msg">{message}</p>
        {error != null && <p className="m-error">{error}</p>}
        <div className="m-tagedit-actions">
          <button type="button" onClick={() => close()} disabled={busy}>
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
