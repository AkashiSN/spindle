import { describe, expect, it } from 'vitest'
import { connectEvents, type EventHandlers, type EventSourceLike } from './events'

class FakeSource implements EventSourceLike {
  onopen: ((ev: Event) => void) | null = null
  onerror: ((ev: Event) => void) | null = null
  listeners = new Map<string, (ev: Event) => void>()
  closed = false
  readonly url: string
  constructor(url: string) {
    this.url = url
  }
  addEventListener(type: string, listener: (ev: Event) => void) {
    this.listeners.set(type, listener)
  }
  close() {
    this.closed = true
  }
  emit(type: string, data: unknown) {
    this.listeners.get(type)?.({ data: JSON.stringify(data) } as unknown as Event)
  }
  open() {
    this.onopen?.({} as Event)
  }
  error() {
    this.onerror?.({} as Event)
  }
}

describe('connectEvents', () => {
  it('初回 open と再接続を区別し、種別ごとに振り分ける', () => {
    const log: string[] = []
    let src: FakeSource | null = null
    const handlers: EventHandlers = {
      onOpen: (re) => log.push(`open:${re}`),
      onError: () => log.push('error'),
      onJob: (e) => log.push(`job:${e.id}:${e.state}`),
      onBatch: (e) => log.push(`batch:${e.id}`),
      onLibrary: (e) => log.push(`library:${e.kind}`),
      onResync: (e) => log.push(`resync:${e.skipped}`),
    }
    const close = connectEvents(
      () => handlers,
      (url) => (src = new FakeSource(url)),
    )
    expect(src!.url).toBe('/api/events')
    src!.open()
    src!.emit('job', { id: 3, state: 'running', progress: 0.5, done: 1, total: 2 })
    src!.emit('library', { kind: 'bulk', scan_run_id: 1 })
    src!.emit('batch', { id: 42, state: 'applied', applied: 1, conflict: 0, failed: 0 })
    src!.emit('resync', { skipped: 7 })
    // 切断 → ブラウザが再接続 → onopen が再び呼ばれる（reconnect=true）
    src!.error()
    src!.open()
    expect(log).toEqual([
      'open:false',
      'job:3:running',
      'library:bulk',
      'batch:42',
      'resync:7',
      'error',
      'open:true',
    ])
    close()
    expect(src!.closed).toBe(true)
  })

  it('壊れた JSON は無視し、ハンドラは最新のものが使われる', () => {
    const log: string[] = []
    let handlers: EventHandlers = { onJob: () => log.push('old') }
    let src: FakeSource | null = null
    connectEvents(
      () => handlers,
      (url) => (src = new FakeSource(url)),
    )
    src!.listeners.get('job')?.({ data: 'not json' } as unknown as Event)
    handlers = { onJob: () => log.push('new') }
    src!.emit('job', { id: 1, state: 'done', progress: null, done: null, total: null })
    expect(log).toEqual(['new'])
  })
})
