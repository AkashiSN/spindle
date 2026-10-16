import { describe, expect, it } from 'vitest'
import type { AdbProgress, JobEvent, SyncProgress } from '../api/types'
import {
  adbProgressFrom,
  agoText,
  applyJobProgress,
  applyProgressEvent,
  mergeListProgress,
  phaseLabel,
  ProgressEventSeq,
  progressAgeSecs,
  progressRatio,
  progressSummary,
  STALE_SECS,
} from './syncProgress'

const MB = 1024 * 1024
function p(over: Partial<SyncProgress> = {}): SyncProgress {
  return {
    plan_id: 1,
    done: 12,
    total: 92,
    bytes_done: 85.3 * MB,
    bytes_total: 634.8 * MB,
    rate: 12.4 * MB,
    phase: 'fetch',
    current: null,
    updated_at: 0,
    age: 0,
    ...over,
  }
}

describe('progressRatio', () => {
  it('bytes_total があればバイトで割る', () => {
    expect(progressRatio(p({ bytes_done: 50, bytes_total: 200 }))).toBe(0.25)
  })
  it('bytes_total が 0 なら件数で割る', () => {
    expect(progressRatio(p({ bytes_total: 0, bytes_done: 0, done: 1, total: 4 }))).toBe(0.25)
  })
  it('どちらも 0 なら 0、範囲は 0..1', () => {
    expect(progressRatio(p({ bytes_total: 0, total: 0, done: 0 }))).toBe(0)
    expect(progressRatio(p({ bytes_done: 300, bytes_total: 200 }))).toBe(1)
  })
})

describe('progressAgeSecs', () => {
  it('受信時刻からの経過秒（切り捨て）', () => {
    expect(progressAgeSecs({ ...p(), receivedAt: 10_000 }, 41_900)).toBe(31)
  })
  it('受信時刻が無ければサーバの age', () => {
    expect(progressAgeSecs(p({ age: 7 }), 50_000)).toBe(7)
  })
  it('時計が戻っても負にしない', () => {
    expect(progressAgeSecs({ ...p(), receivedAt: 60_000 }, 50_000)).toBe(0)
  })
})

describe('progressSummary', () => {
  it('新しい進捗は速度と残りを出す', () => {
    expect(progressSummary(p(), 0)).toBe('12 / 92 件 · 85.3 MB / 634.8 MB · 12.4 MB/s · 残り 0:44')
  })
  it('30 秒ちょうどはまだ新しい', () => {
    expect(progressSummary(p(), STALE_SECS)).toContain('MB/s')
  })
  it('31 秒は応答なし', () => {
    expect(progressSummary(p(), 31)).toBe('12 / 92 件 · 85.3 MB / 634.8 MB · 応答なし（最後の進捗: 31 秒前）')
  })
  it('bytes_total が 0 なら件数だけ', () => {
    expect(progressSummary(p({ bytes_total: 0, bytes_done: 0 }), 0)).toBe('12 / 92 件')
  })
  it('stale: false なら 30 秒を超えても応答なしを出さない（Android）', () => {
    const out = progressSummary(p(), 120, { stale: false })
    expect(out).not.toContain('応答なし')
    expect(out).toBe('12 / 92 件 · 85.3 MB / 634.8 MB · 12.4 MB/s · 残り 0:44')
  })
  it('rate が 0 なら速度と残りを出さない', () => {
    expect(progressSummary(p({ rate: 0 }), 0)).toBe('12 / 92 件 · 85.3 MB / 634.8 MB')
  })
})

describe('agoText', () => {
  it('秒・分・時間', () => {
    expect(agoText(45)).toBe('45 秒前')
    expect(agoText(60)).toBe('1 分前')
    expect(agoText(125)).toBe('2 分前')
    expect(agoText(3600)).toBe('1 時間前')
  })
})

describe('phaseLabel', () => {
  it('段階の名前', () => {
    expect(phaseLabel('delete')).toBe('削除中')
    expect(phaseLabel('move')).toBe('移動中')
    expect(phaseLabel('fetch')).toBe('受信中')
    expect(phaseLabel('add')).toBe('追加中')
    expect(phaseLabel('playlist')).toBe('プレイリスト')
    expect(phaseLabel('send')).toBe('送信中')
  })
})

describe('ProgressEventSeq', () => {
  it('印の後に届いたイベントだけを新しいとみなす（時刻に依らない）', () => {
    const s = new ProgressEventSeq()
    s.note(1)
    const mark = s.mark()
    expect(s.since(1, mark)).toBe(false)
    expect(s.since(2, mark)).toBe(false)
    // 要求と同じ 1 ms に届いても、印の後なら新しい
    s.note(2)
    expect(s.since(2, mark)).toBe(true)
    expect(s.since(1, mark)).toBe(false)
    s.note(1)
    expect(s.since(1, mark)).toBe(true)
  })
})

describe('mergeListProgress', () => {
  const resp = [{ id: 1, sync_progress: p({ age: 2 }) }]
  it('要求より新しいイベントがあれば現在の値（null 含む）を残す', () => {
    const newerEv = () => true
    expect(mergeListProgress(resp, [{ id: 1, sync_progress: null }], newerEv, 2000)[0].sync_progress).toBeNull()
    const newer = p({ done: 50 })
    expect(mergeListProgress(resp, [{ id: 1, sync_progress: newer }], newerEv, 2000)[0].sync_progress).toBe(newer)
  })
  it('要求と同じ時刻に届いた null も、要求の後なら応答に負けない', () => {
    const s = new ProgressEventSeq()
    const mark = s.mark()
    s.note(1)
    const out = mergeListProgress(resp, [{ id: 1, sync_progress: null }], (d) => s.since(d, mark), 1000)
    expect(out[0].sync_progress).toBeNull()
  })
  it('要求より古いイベントは応答に負ける（receivedAt を付ける）', () => {
    const s = new ProgressEventSeq()
    s.note(1)
    const mark = s.mark()
    const out = mergeListProgress(resp, [{ id: 1, sync_progress: null }], (d) => s.since(d, mark), 5000)[0].sync_progress
    expect(out?.done).toBe(12)
    expect((out as { receivedAt?: number }).receivedAt).toBe(3000)
  })
  it('イベントが無い端末は応答のまま', () => {
    expect(mergeListProgress(resp, null, () => false, 5000)[0].sync_progress?.done).toBe(12)
  })
})

describe('applyProgressEvent', () => {
  type Row = { id: number; plan_open: boolean; sync_progress: (SyncProgress & { receivedAt?: number }) | null }
  const items: Row[] = [
    { id: 1, plan_open: false, sync_progress: null },
    { id: 2, plan_open: false, sync_progress: null },
  ]
  it('進捗が来たら plan_open を立て、受信時刻を付ける（確定はイベントを流さない）', () => {
    const out = applyProgressEvent(items, { device_id: 1, progress: p({ age: 2 }) }, 5000)
    expect(out[0].plan_open).toBe(true)
    expect(out[0].sync_progress?.receivedAt).toBe(3000)
    expect(out[1]).toBe(items[1])
  })
  it('null は進捗だけ消し、plan_open に触らない', () => {
    const open: Row[] = [{ id: 1, plan_open: true, sync_progress: p() }]
    const out = applyProgressEvent(open, { device_id: 1, progress: null }, 5000)
    expect(out[0].sync_progress).toBeNull()
    expect(out[0].plan_open).toBe(true)
  })
  it('一覧に無い端末は無視する', () => {
    const out = applyProgressEvent(items, { device_id: 9, progress: p() }, 5000)
    expect(out).toEqual(items)
  })
})

function ev(over: Partial<JobEvent> = {}): JobEvent {
  return {
    id: 7,
    state: 'running',
    progress: 0.1,
    done: 3,
    total: 30,
    detail: { kind: 'device_sync', phase: 'send', current: 'x.opus', bytes_done: 100, bytes_total: 1000, rate: 50 },
    ...over,
  }
}

describe('adbProgressFrom', () => {
  it('device_sync の detail と件数から進捗を作る', () => {
    expect(adbProgressFrom(ev())).toEqual({
      done: 3,
      total: 30,
      bytes_done: 100,
      bytes_total: 1000,
      rate: 50,
      phase: 'send',
      current: 'x.opus',
    })
  })
  it('current が null でもよい', () => {
    const d = { kind: 'device_sync', phase: 'delete', current: null, bytes_done: 0, bytes_total: 0, rate: 0 }
    expect(adbProgressFrom(ev({ detail: d }))?.current).toBeNull()
  })
  it('kind 違い・数値の欠け・未知の phase・件数の null は null', () => {
    const base = { kind: 'device_sync', phase: 'send', current: null, bytes_done: 1, bytes_total: 2, rate: 0 }
    expect(adbProgressFrom(ev({ detail: { ...base, kind: 'rip' } }))).toBeNull()
    expect(adbProgressFrom(ev({ detail: { ...base, bytes_done: undefined } }))).toBeNull()
    expect(adbProgressFrom(ev({ detail: { ...base, rate: 'x' } }))).toBeNull()
    expect(adbProgressFrom(ev({ detail: { ...base, phase: 'fetch' } }))).toBeNull()
    expect(adbProgressFrom(ev({ detail: base, done: null }))).toBeNull()
    expect(adbProgressFrom(ev({ detail: base, total: null }))).toBeNull()
    expect(adbProgressFrom(ev({ detail: undefined }))).toBeNull()
    expect(adbProgressFrom(ev({ detail: 'device_sync' }))).toBeNull()
  })
})

describe('applyJobProgress', () => {
  const prog: AdbProgress = {
    done: 1,
    total: 2,
    bytes_done: 0,
    bytes_total: 0,
    rate: 0,
    phase: 'delete',
    current: null,
  }
  it('running で detail が読めれば入れる（新しい表を返す）', () => {
    const m = new Map<number, AdbProgress>()
    const out = applyJobProgress(m, ev())
    expect(out).not.toBe(m)
    expect(out.get(7)?.phase).toBe('send')
    expect(m.size).toBe(0)
  })
  it('終端の状態なら消す', () => {
    for (const state of ['done', 'failed', 'cancelled'] as const) {
      const m = new Map([[7, prog]])
      const out = applyJobProgress(m, ev({ state, detail: undefined }))
      expect(out.has(7)).toBe(false)
    }
  })
  it('detail が読めない running は表を変えない（同じ表を返す）', () => {
    const m = new Map([[7, prog]])
    expect(applyJobProgress(m, ev({ detail: undefined }))).toBe(m)
  })
  it('queued は表を変えない', () => {
    const m = new Map([[7, prog]])
    expect(applyJobProgress(m, ev({ state: 'queued', detail: undefined }))).toBe(m)
  })
  it('表に無いジョブの終端は同じ表を返す', () => {
    const m = new Map([[8, prog]])
    expect(applyJobProgress(m, ev({ state: 'done' }))).toBe(m)
  })
})
