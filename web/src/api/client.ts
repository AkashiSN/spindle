// fetch の薄いラッパ。401 は `onUnauthorized` に通知してログイン画面へ切り替える（D-35）。
// 変更系は同一オリジンから送るので Origin / Sec-Fetch-Site はブラウザが付ける（CSRF 検証はサーバ側）

import type { ErrorBody } from './types'

export class ApiError extends Error {
  readonly status: number
  readonly code: string
  constructor(status: number, code: string, message?: string) {
    super(message ?? `${code} (HTTP ${status})`)
    this.status = status
    this.code = code
  }
}

type Listener = () => void
const unauthorizedListeners = new Set<Listener>()

/** セッション切れ（401）の通知先を登録する */
export function onUnauthorized(listener: Listener): () => void {
  unauthorizedListeners.add(listener)
  return () => unauthorizedListeners.delete(listener)
}

async function parseError(res: Response): Promise<ApiError> {
  let body: ErrorBody | null = null
  try {
    body = (await res.json()) as ErrorBody
  } catch {
    // JSON でない本文（proxy のエラーページ等）
  }
  return new ApiError(res.status, body?.error ?? 'http_error', body?.message)
}

export async function apiFetch<T>(
  path: string,
  init?: RequestInit & { signal?: AbortSignal },
): Promise<T> {
  const res = await fetch(path, {
    credentials: 'same-origin',
    ...init,
    headers: { Accept: 'application/json', ...(init?.headers ?? {}) },
  })
  if (res.status === 401 && !path.startsWith('/api/auth/login')) {
    for (const l of unauthorizedListeners) l()
  }
  if (!res.ok) throw await parseError(res)
  if (res.status === 204) return undefined as T
  return (await res.json()) as T
}

export async function apiPost<T>(path: string, body: unknown): Promise<T> {
  return apiFetch<T>(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })
}
