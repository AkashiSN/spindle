import { afterEach, describe, expect, it, vi } from 'vitest'
import { ApiError, apiFetch, apiPost } from './client'

function respond(status: number, body: string, headers: Record<string, string> = {}) {
  return new Response(body, { status, headers })
}

describe('apiFetch', () => {
  afterEach(() => vi.unstubAllGlobals())

  it('treats an empty 202 body as success (cancel endpoints)', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => respond(202, '')))
    await expect(apiPost<void>('/api/history/1/cancel', {})).resolves.toBeUndefined()
  })

  it('parses a JSON body', async () => {
    vi.stubGlobal(
      'fetch',
      vi.fn(async () => respond(201, '{"batch_id":7}', { 'Content-Type': 'application/json' })),
    )
    await expect(apiPost<{ batch_id: number }>('/api/history/1/revert', {})).resolves.toEqual({
      batch_id: 7,
    })
  })

  it('turns an error body into ApiError with the code', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => respond(409, '{"error":"not_cancellable"}')))
    const err = await apiFetch('/api/history/1/cancel', { method: 'POST' }).catch((e: unknown) => e)
    expect(err).toBeInstanceOf(ApiError)
    expect((err as ApiError).code).toBe('not_cancellable')
    expect((err as ApiError).status).toBe(409)
  })
})
