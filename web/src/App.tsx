import { useEffect, useState } from 'react'

type Health = { status: string }

// P0-1 の骨格。3 ペインの表 UI は P0-8 で載せる
function App() {
  const [health, setHealth] = useState<Health | null>(null)
  const [error, setError] = useState<string | null>(null)

  useEffect(() => {
    fetch('/health')
      .then((res) => {
        if (!res.ok) throw new Error(`HTTP ${res.status}`)
        return res.json() as Promise<Health>
      })
      .then(setHealth)
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
  }, [])

  return (
    <main>
      <h1>spindle</h1>
      <p>
        サーバ状態:{' '}
        {error ? <span className="error">取得失敗（{error}）</span> : (health?.status ?? '確認中…')}
      </p>
    </main>
  )
}

export default App
