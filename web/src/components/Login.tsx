// ログイン画面（SPEC §9 認証、D-35）。GET /api/auth/session が 401 のときだけ出る

import { useState, type FormEvent } from 'react'
import { ApiError, apiPost } from '../api/client'

export function Login({ onLoggedIn }: { onLoggedIn: () => void }) {
  const [password, setPassword] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const submit = async (e: FormEvent) => {
    e.preventDefault()
    setBusy(true)
    setError(null)
    try {
      await apiPost('/api/auth/login', { password })
      setPassword('')
      onLoggedIn()
    } catch (err) {
      if (err instanceof ApiError) {
        setError(
          err.status === 401
            ? 'パスワードが違います'
            : err.status === 429
              ? '試行回数が多すぎます。しばらく待ってください'
              : err.status === 503
                ? 'パスワードが未設定です。SPINDLE_INITIAL_PASSWORD を設定して起動し直してください'
                : `ログインに失敗（${err.code}）`,
        )
      } else {
        setError('ログインに失敗（ネットワーク）')
      }
    } finally {
      setBusy(false)
    }
  }

  return (
    <main className="login">
      <form onSubmit={submit}>
        <h1>spindle</h1>
        <label>
          パスワード
          <input
            type="password"
            autoFocus
            autoComplete="current-password"
            value={password}
            onChange={(e) => setPassword(e.target.value)}
          />
        </label>
        <button type="submit" disabled={busy || password.length === 0}>
          ログイン
        </button>
        {error && <p className="error">{error}</p>}
      </form>
    </main>
  )
}
