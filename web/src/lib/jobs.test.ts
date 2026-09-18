import { describe, expect, it } from 'vitest'
import type { Job } from '../api/types'
import { canCancel, canRetry, filterJobs, jobProgress, jobTypeLabel, summarizeByType } from './jobs'

function job(over: Partial<Job>): Job {
  return {
    id: 1,
    type: 'transcode',
    state: 'queued',
    progress: null,
    done: null,
    total: null,
    attempts: 0,
    max_attempts: 3,
    last_error: null,
    run_after: null,
    edit_batch_id: null,
    created_at: 1,
    started_at: null,
    finished_at: null,
    ...over,
  }
}

describe('summarizeByType', () => {
  it('種別ごとに queued / running / failed を数え、並列度を添える。並列度がある種別は 0 件でも出す', () => {
    const rows = summarizeByType(
      [
        job({ id: 1, type: 'transcode', state: 'queued' }),
        job({ id: 2, type: 'transcode', state: 'running' }),
        job({ id: 3, type: 'transcode', state: 'done' }),
        job({ id: 4, type: 'rg', state: 'failed' }),
        job({ id: 5, type: 'mystery', state: 'queued' }),
      ],
      { transcode: 11, rg: 12, scan: 1 },
    )
    expect(rows.map((r) => r.type)).toEqual(['rg', 'scan', 'transcode', 'mystery'])
    expect(rows.find((r) => r.type === 'transcode')).toEqual({
      type: 'transcode',
      concurrency: 11,
      queued: 1,
      running: 1,
      failed: 0,
    })
    expect(rows.find((r) => r.type === 'scan')).toEqual({ type: 'scan', concurrency: 1, queued: 0, running: 0, failed: 0 })
    expect(rows.find((r) => r.type === 'mystery')).toEqual({ type: 'mystery', concurrency: null, queued: 1, running: 0, failed: 0 })
  })
})

describe('filterJobs', () => {
  const items = [
    job({ id: 1, type: 'transcode', state: 'queued' }),
    job({ id: 2, type: 'rg', state: 'running' }),
    job({ id: 3, type: 'rg', state: 'failed' }),
    job({ id: 4, type: 'scan', state: 'done' }),
    job({ id: 5, type: 'scan', state: 'cancelled' }),
  ]
  it('state: active は queued + running、failed は failed + cancelled、all は全部', () => {
    expect(filterJobs(items, 'active', null).map((j) => j.id)).toEqual([1, 2])
    expect(filterJobs(items, 'failed', null).map((j) => j.id)).toEqual([3, 5])
    expect(filterJobs(items, 'all', null).map((j) => j.id)).toEqual([1, 2, 3, 4, 5])
  })
  it('type で絞る', () => {
    expect(filterJobs(items, 'all', 'rg').map((j) => j.id)).toEqual([2, 3])
  })
})

describe('jobProgress', () => {
  it('done / total があればそれ、progress だけなら %、無ければ空', () => {
    expect(jobProgress(job({ done: 3, total: 10 }))).toBe('3 / 10')
    expect(jobProgress(job({ progress: 0.456 }))).toBe('46%')
    expect(jobProgress(job({}))).toBe('')
  })
})

describe('canCancel / canRetry', () => {
  it('queued / running は取り消せ、failed / cancelled は再試行できる', () => {
    expect(canCancel(job({ state: 'queued' }))).toBe(true)
    expect(canCancel(job({ state: 'running' }))).toBe(true)
    expect(canCancel(job({ state: 'done' }))).toBe(false)
    expect(canRetry(job({ state: 'failed' }))).toBe(true)
    expect(canRetry(job({ state: 'cancelled' }))).toBe(true)
    expect(canRetry(job({ state: 'queued' }))).toBe(false)
  })
})

describe('jobTypeLabel', () => {
  it('既知の種別は日本語、未知はそのまま', () => {
    expect(jobTypeLabel('transcode')).toBe('Derived 生成')
    expect(jobTypeLabel('mystery')).toBe('mystery')
  })
})
