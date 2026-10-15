// ログ本文の表示（記録画面のログタブ・Inbox の吸い出しログ）。等幅・折り返さず横スクロール、
// コピーとダウンロード

import { useState } from 'react'

export function LogText({ text, fileName }: { text: string; fileName: string }) {
  const [copied, setCopied] = useState(false)
  const download = () => {
    const url = URL.createObjectURL(new Blob([text], { type: 'text/plain;charset=utf-8' }))
    const a = document.createElement('a')
    a.href = url
    a.download = fileName
    a.click()
    // ダウンロードの開始を待ってから解放する
    setTimeout(() => URL.revokeObjectURL(url), 0)
  }
  return (
    <div className="log-text">
      <div className="log-text-actions">
        <button
          type="button"
          className="ghost small"
          onClick={() => {
            void navigator.clipboard?.writeText(text).then(() => setCopied(true), () => undefined)
          }}
        >
          {copied ? 'コピーした' : 'コピー'}
        </button>
        <button type="button" className="ghost small" onClick={download}>
          ダウンロード
        </button>
      </div>
      <pre>{text}</pre>
    </div>
  )
}
