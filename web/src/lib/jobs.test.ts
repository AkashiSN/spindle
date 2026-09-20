import { describe, expect, it } from 'vitest'
import type { Job } from '../api/types'
import {
  canCancel,
  canRetry,
  cpuBudgetLabel,
  filterJobs,
  jobProgress,
  jobTypeLabel,
  shownLimit,
  summarizeByType,
  tabCounts,
} from './jobs'

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
    subject: null,
    ...over,
  }
}

describe('summarizeByType', () => {
  it('サーバの種別ごとの件数に並列度を添える。並列度がある種別は 0 件でも出す（一覧は上限付きなので数えない）', () => {
    const rows = summarizeByType(
      {
        transcode: { queued: 6470, running: 11, done: 1089, failed: 0, cancelled: 0 },
        rg: { queued: 0, running: 0, done: 0, failed: 1, cancelled: 2 },
        mystery: { queued: 1, running: 0, done: 0, failed: 0, cancelled: 0 },
      },
      { transcode: 11, rg: 12, scan: 1 },
    )
    expect(rows.map((r) => r.type)).toEqual(['rg', 'scan', 'transcode', 'mystery'])
    expect(rows.find((r) => r.type === 'transcode')).toEqual({
      type: 'transcode',
      concurrency: 11,
      queued: 6470,
      running: 11,
      done: 1089,
      failed: 0,
      cancelled: 0,
    })
    expect(rows.find((r) => r.type === 'rg')?.cancelled).toBe(2)
    expect(rows.find((r) => r.type === 'scan')).toEqual({
      type: 'scan',
      concurrency: 1,
      queued: 0,
      running: 0,
      done: 0,
      failed: 0,
      cancelled: 0,
    })
    expect(rows.find((r) => r.type === 'mystery')).toEqual({
      type: 'mystery',
      concurrency: null,
      queued: 1,
      running: 0,
      done: 0,
      failed: 0,
      cancelled: 0,
    })
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
  it('state: active は queued + running、done は done、failed は failed + cancelled、all は全部', () => {
    expect(filterJobs(items, 'active', null).map((j) => j.id)).toEqual([1, 2])
    expect(filterJobs(items, 'done', null).map((j) => j.id)).toEqual([4])
    expect(filterJobs(items, 'failed', null).map((j) => j.id)).toEqual([3, 5])
    expect(filterJobs(items, 'all', null).map((j) => j.id)).toEqual([1, 2, 3, 4, 5])
  })
  it('上限に達したタブだけ「表示は最新 N 件まで」', () => {
    const limits = { active: 2, done: 1, failed: 5 }
    const shown = filterJobs(items, 'all', null)
    expect(shownLimit('active', limits, filterJobs(items, 'active', null))).toBe(2)
    expect(shownLimit('done', limits, filterJobs(items, 'done', null))).toBe(1)
    expect(shownLimit('failed', limits, filterJobs(items, 'failed', null))).toBeNull()
    expect(shownLimit('all', limits, shown)).toBe(8)
    expect(shownLimit('all', { active: 9, done: 9, failed: 9 }, shown)).toBeNull()
    expect(shownLimit('active', null, shown)).toBeNull()
  })
  it('タブの件数は summary の全件集計から（一覧は上限付き）', () => {
    const summary = { running: 12, queued: 8835, done: 15300, failed: 2, cancelled: 3, pending_ops: 0 }
    expect(tabCounts(summary)).toEqual({ active: 8847, done: 15300, failed: 5, all: 24152 })
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

describe('cpuBudgetLabel（D-73）', () => {
  it('CPU 系の実行中の合計と予算を出し、予算が無ければ null', () => {
    const z = { done: 0, failed: 0, cancelled: 0 }
    const rows = summarizeByType(
      {
        transcode: { queued: 0, running: 1, ...z },
        rg: { queued: 0, running: 1, ...z },
        thumbnail: { queued: 0, running: 1, ...z },
        flaccheck: { queued: 1, running: 0, ...z },
      },
      { transcode: 11, rg: 12, thumbnail: 4, flaccheck: 12 },
    )
    expect(cpuBudgetLabel(rows, 12)).toBe(
      'CPU 系（rg / transcode / flaccheck / hirescheck）の実行中の合計 2 / 予算 12（= コア数）',
    )
    expect(cpuBudgetLabel(rows, null)).toBeNull()
  })
})
