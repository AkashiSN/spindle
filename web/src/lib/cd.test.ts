import { describe, expect, it } from 'vitest'
import {
  candidateLengthMs,
  candidateSummary,
  initialSelection,
  lookupHeadline,
  normalizeTocInput,
  outcomeAfterTocEdit,
  type ReleaseCandidate,
} from './cd'

const base: ReleaseCandidate = {
  release_id: 'r',
  release_group_id: null,
  title: 'T',
  artist: 'A',
  date: '1991-09-24',
  country: 'US',
  status: 'Official',
  barcode: '720642442524',
  disambiguation: null,
  labels: [['DGC Records', 'DGCD-24425']],
  exact: true,
  medium_position: 1,
  medium_count: 1,
  medium_title: null,
  format: 'CD',
  tracks: [
    { number: '1', position: 1, title: 'a', artist: 'A', length_ms: 1000, recording_id: 'x', track_id: 'y', isrcs: [] },
    { number: '2', position: 2, title: 'b', artist: 'A', length_ms: 2500, recording_id: 'x', track_id: 'y', isrcs: [] },
  ],
}

describe('normalizeTocInput', () => {
  it('CTDB / MusicBrainz 形式はそのまま（前後の空白は落とす）', () => {
    expect(normalizeTocInput('  0:13915:25592:241060 \n')).toBe('0:13915:25592:241060')
    expect(normalizeTocInput('1 6 95462 150 15363')).toBe('1 6 95462 150 15363')
  })
  it('cdrecord -toc の出力から LBA を拾い、データトラックに - を付ける', () => {
    const out = `first: 1 last 8
track:   1 lba:         0 (        0) 00:02:00 adr: 1 control: 0 mode: 0
track:   2 lba:     13959 (    55836) 03:08:09 adr: 1 control: 0 mode: 0
track:   8 lba:    125824 (   503296) 27:59:49 adr: 1 control: 6 mode: 1
track:lout lba:    188333 (   753332) 41:53:08 adr: 1 control: 6 mode: -1`
    expect(normalizeTocInput(out)).toBe('0:13959:-125824:188333')
  })
  it('リードアウトが無ければ手を付けない', () => {
    expect(normalizeTocInput('track: 1 lba: 0')).toBe('track: 1 lba: 0')
  })
})

describe('candidateSummary', () => {
  it('日付・国・レーベル・バーコード・形式を並べる', () => {
    expect(candidateSummary(base)).toBe('1991-09-24 · US · DGC Records DGCD-24425 · JAN/UPC 720642442524 · CD')
  })
  it('複数枚組は n/m、非公式と注記も出す', () => {
    expect(
      candidateSummary({
        ...base,
        date: null,
        country: null,
        labels: [['Sub Pop', null]],
        barcode: null,
        medium_position: 2,
        medium_count: 3,
        status: 'Bootleg',
        disambiguation: 'first press',
      }),
    ).toBe('Sub Pop · CD 2/3 · Bootleg · first press')
  })
})

describe('candidateLengthMs / lookupHeadline', () => {
  it('長さの合計。不明があれば null', () => {
    expect(candidateLengthMs(base)).toBe(3500)
    expect(candidateLengthMs({ ...base, tracks: [{ ...base.tracks[0]!, length_ms: null }] })).toBeNull()
  })
  it('見出しは exact / fuzzy / 0 件で変える', () => {
    const r = { discid: 'd', mb_toc: '', accuraterip_id: '', ctdb_toc_id: '', exact: true, candidates: [base] }
    expect(lookupHeadline(r)).toBe('DiscID が一致: 1 件')
    expect(lookupHeadline({ ...r, exact: false, candidates: [{ ...base, exact: false }] })).toBe(
      'TOC の近い候補（DiscID は未登録）: 1 件',
    )
    // fuzzy 経路でも候補側に DiscID 一致があれば「未登録」と言わない
    expect(lookupHeadline({ ...r, exact: false, candidates: [base, { ...base, exact: false }] })).toBe(
      'TOC で照会（DiscID の一致する候補 1 件を含む）: 2 件',
    )
    expect(lookupHeadline({ ...r, exact: false, candidates: [] })).toBe('MusicBrainz に見つからない（手入力へ。P2-4）')
  })
})

describe('状態遷移', () => {
  const r = { discid: 'd', mb_toc: '', accuraterip_id: '', ctdb_toc_id: '', exact: true, candidates: [base] }
  it('TOC を編集したら結果と選択を捨てる。同じ入力なら保つ', () => {
    const outcome = { result: r, selected: 0, error: null }
    expect(outcomeAfterTocEdit('a', 'b', outcome)).toEqual({ result: null, selected: null, error: null })
    expect(outcomeAfterTocEdit('a', 'a', outcome)).toBe(outcome)
  })
  it('照会に失敗した後（結果なし・エラーあり）に TOC を編集したらエラーも消す', () => {
    const failed = { result: null, selected: null, error: 'MusicBrainz に届かない' }
    expect(outcomeAfterTocEdit('a', 'b', failed)).toEqual({ result: null, selected: null, error: null })
    expect(outcomeAfterTocEdit('a', 'a', failed)).toBe(failed)
  })
  it('DiscID 一致がちょうど 1 件なら選んでおく', () => {
    expect(initialSelection(r)).toBe(0)
    expect(initialSelection({ ...r, candidates: [base, base] })).toBeNull()
    expect(initialSelection({ ...r, candidates: [{ ...base, exact: false }] })).toBeNull()
    expect(initialSelection({ ...r, candidates: [{ ...base, exact: false }, base] })).toBe(1)
  })
})
