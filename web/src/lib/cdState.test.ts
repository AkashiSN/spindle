import { describe, expect, it } from 'vitest'
import type { LookupResponse, ReleaseCandidate } from './cd'
import { cdReducer, initialCdState, type CdAction, type CdState } from './cdState'

const cand = (exact: boolean, title = 'T'): ReleaseCandidate => ({
  release_id: `r-${title}`,
  release_group_id: null,
  title,
  artist: 'A',
  date: null,
  country: null,
  status: null,
  barcode: null,
  disambiguation: null,
  labels: [],
  exact,
  matched_by: exact ? ['discid'] : ['toc'],
  media: [{ position: 1, format: 'CD', track_count: 2 }],
  medium_position: 1,
  medium_count: 1,
  medium_title: null,
  format: 'CD',
  tracks: [
    { number: '1', position: 1, title: 'a', artist: 'A', length_ms: 1000, recording_id: 'x', track_id: 'y', isrcs: [] },
    { number: '2', position: 2, title: 'b', artist: 'A', length_ms: 2500, recording_id: 'x', track_id: 'y', isrcs: [] },
  ],
})

const tracks = [
  { number: 1, length_ms: 1000 },
  { number: 2, length_ms: 2500 },
]

const response = (candidates: ReleaseCandidate[], exact = candidates.some((c) => c.exact)): LookupResponse => ({
  discid: 'd',
  mb_toc: '',
  accuraterip_id: '',
  ctdb_toc_id: '',
  stage: 'discid' as const,
  can_widen: false,
  exact,
  candidates,
  notes: [],
  tracks,
})

function run(actions: CdAction[], from: CdState = initialCdState): CdState {
  return actions.reduce(cdReducer, from)
}

const looked = (r: LookupResponse) => run([{ type: 'set_toc', toc: '0:1000:5000' }, { type: 'lookup_start' }, { type: 'lookup_ok', result: r }])

describe('cdReducer: 照会', () => {
  it('lookup_start で busy。古い結果と選択は消すが、フォームは残す（P4-20）', () => {
    const s = cdReducer({ ...initialCdState, result: response([]), selected: 0, error: 'e' }, { type: 'lookup_start' })
    expect(s).toMatchObject({ busy: true, error: null, result: null, selected: null })
  })
  it('候補ゼロ件なら空のフォームに直行（照会ゼロ件でも完走できる）', () => {
    const s = looked(response([]))
    expect(s.busy).toBe(false)
    expect(s.selected).toBeNull()
    expect(s.draft).toMatchObject({ source: 'manual', album: '' })
    expect(s.draft!.tracks.map((t) => t.number)).toEqual([1, 2])
  })
  it('exact がちょうど 1 件ならその候補をフォームに', () => {
    const s = looked(response([cand(false, 'X'), cand(true, 'Y')]))
    expect(s.selected).toBe(1)
    expect(s.draft).toMatchObject({ source: 'musicbrainz', album: 'Y', release_id: 'r-Y' })
  })
  it('exact が複数、または近似だけなら候補は選ばない（表は TOC から出たまま残る）', () => {
    const many = looked(response([cand(true), cand(true)]))
    expect(many.selected).toBeNull()
    expect(many.draft).toMatchObject({ source: 'manual', album: '' })
    const fuzzy = looked(response([cand(false)]))
    expect(fuzzy.selected).toBeNull()
    expect(fuzzy.draft).toMatchObject({ source: 'manual', album: '' })
  })
  it('lookup_error は結果を消してエラーを出す（フォームは残す）', () => {
    const s = run([{ type: 'lookup_start' }, { type: 'lookup_error', error: 'x' }])
    expect(s).toMatchObject({ busy: false, error: 'x', result: null })
  })
})

describe('cdReducer: 選択と「どれも違う」', () => {
  const two = response([cand(false, 'X'), cand(false, 'Y')])
  it('select で候補を写し、選び直すと写し直す', () => {
    let s = cdReducer(looked(two), { type: 'select', index: 0 })
    expect(s.draft).toMatchObject({ album: 'X', source: 'musicbrainz' })
    s = cdReducer(s, { type: 'select', index: 1 })
    expect(s.selected).toBe(1)
    expect(s.draft).toMatchObject({ album: 'Y' })
  })
  it('start_manual（どれも違う）で名前の無いフォームに戻り、selected は外れる', () => {
    const s = run([{ type: 'select', index: 0 }, { type: 'start_manual' }], looked(two))
    expect(s.selected).toBeNull()
    expect(s.draft).toMatchObject({ source: 'manual', album: '' })
  })
  it('範囲外の select と、結果が無いときの select / start_manual は何もしない', () => {
    const s = looked(two)
    expect(cdReducer(s, { type: 'select', index: 9 })).toBe(s)
    expect(cdReducer(initialCdState, { type: 'select', index: 0 })).toBe(initialCdState)
    expect(cdReducer(initialCdState, { type: 'start_manual' })).toBe(initialCdState)
  })
})

describe('cdReducer: 候補から写す範囲（D-72、P4-2）', () => {
  const two = response([cand(true, 'X'), cand(false, 'Y')])
  it('既定は full: exact の自動選択も select もトラック名まで写す（P4-20）', () => {
    const s = looked(two)
    expect(s.copyScope).toBe('full')
    expect(s.selected).toBe(0)
    expect(s.draft).toMatchObject({ album: 'X' })
    expect(s.draft!.tracks[0]).toMatchObject({ title: 'a', artist: 'A' })
    const t = cdReducer(s, { type: 'select', index: 1 })
    expect(t.draft).toMatchObject({ album: 'Y' })
    expect(t.draft!.tracks[0]).toMatchObject({ title: 'a' })
  })
  it('set_copy_scope: minimal で選択中の候補を写し直すと空行になる', () => {
    let s = cdReducer(looked(two), { type: 'set_copy_scope', scope: 'minimal' })
    expect(s.copyScope).toBe('minimal')
    expect(s.selected).toBe(0)
    expect(s.draft!.album).toBe('X')
    expect(s.draft).toMatchObject({ label: '', catalog_number: '', barcode: '' })
    expect(s.draft!.tracks[0]).toMatchObject({ title: '', artist: '', mb: null })
    s = cdReducer(s, { type: 'set_copy_scope', scope: 'full' })
    expect(s.draft!.tracks[0]).toMatchObject({ title: 'a', artist: 'A', mb: { recording_id: 'x', track_id: 'y', isrcs: [] } })
  })
  it('同じ範囲なら何もしない。手入力中は範囲だけ変わる。確定後も範囲だけ変わりフォームは残る', () => {
    const s = looked(two)
    expect(cdReducer(s, { type: 'set_copy_scope', scope: 'full' })).toBe(s)
    const manual = cdReducer(s, { type: 'start_manual' })
    const m2 = cdReducer(manual, { type: 'set_copy_scope', scope: 'full' })
    expect(m2.copyScope).toBe('full')
    expect(m2.draft).toBe(manual.draft)
  })
})

describe('cdReducer: ディスクの出し入れ（P4-20）', () => {
  // 候補を選んで名前の入った状態を作る（画面からは編集できないので select で作る）
  const filled = cdReducer(looked(response([cand(false, 'X')])), { type: 'select', index: 0 })
  it('ディスクが入ったら照会の前からフォームができる', () => {
    const s = cdReducer(initialCdState, { type: 'set_disc', toc: '0:1000:2500', tracks })
    expect(s.toc).toBe('0:1000:2500')
    expect(s.draft?.tracks).toHaveLength(2)
    expect(s.draft?.tracks[0]?.title).toBe('')
    expect(s.result).toBeNull()
  })
  it('同じディスクの set_disc は編集中の内容を消さない（2 秒ごとのポーリング）', () => {
    let s = cdReducer(initialCdState, { type: 'set_disc', toc: '0:1000:2500', tracks })
    const again = cdReducer(s, { type: 'set_disc', toc: '0:1000:2500', tracks })
    expect(again).toBe(s)
  })
  it('別のディスクに替わるとフォームごと作り直す', () => {
    let s = cdReducer(initialCdState, { type: 'set_disc', toc: '0:1000:2500', tracks })
    s = cdReducer(s, { type: 'set_disc', toc: '0:3000', tracks: [{ number: 1, length_ms: 40000 }] })
    expect(s.draft?.album).toBe('')
    expect(s.draft?.tracks).toHaveLength(1)
  })
  it('照会を始めても表は消えない', () => {
    const s = cdReducer(filled, { type: 'lookup_start' })
    expect(s.draft).toBe(filled.draft)
    expect(s.busy).toBe(true)
  })
  it('照会に失敗しても表と写した内容は残る', () => {
    let s = cdReducer(filled, { type: 'lookup_start' })
    s = cdReducer(s, { type: 'lookup_error', error: 'MusicBrainz に届かない' })
    expect(s.draft?.album).toBe('X')
    expect(s.error).toBe('MusicBrainz に届かない')
    expect(s.busy).toBe(false)
  })
  it('照会中に結果を消しても busy が残らない', () => {
    let s = cdReducer(filled, { type: 'lookup_start' })
    s = cdReducer(s, { type: 'reset' })
    expect(s.busy).toBe(false)
    expect(s.draft).toBeNull()
  })
  it('照会中に別のディスクへ替わったら、古い結果も busy も消える', () => {
    let s = cdReducer(filled, { type: 'lookup_start' })
    s = cdReducer(s, { type: 'set_disc', toc: '0:9999', tracks: [{ number: 1, length_ms: 1 }] })
    expect(s.busy).toBe(false)
    expect(s.result).toBeNull()
  })
  it('TOC を編集すると結果とフォームが消える（同じ TOC なら保つ）', () => {
    const same = cdReducer(filled, { type: 'set_toc', toc: filled.toc })
    expect(same.draft).toBe(filled.draft)
    const edited = cdReducer(filled, { type: 'set_toc', toc: '0:2000' })
    expect(edited).toMatchObject({ toc: '0:2000', result: null, draft: null, selected: null })
  })
  it('照会をやり直しても、候補ゼロ件なら TOC から作ったフォームが残る', () => {
    const s = run([{ type: 'lookup_start' }, { type: 'lookup_ok', result: response([]) }], filled)
    expect(s.draft).toBe(filled.draft)
  })
})

describe('cdReducer: 手入力の品番と JAN（D-94）', () => {
  const shokai: ReleaseCandidate = {
    ...cand(true, 'S'),
    labels: [['L', 'UPCJ-9001']],
    barcode: '4988031278079',
  }
  const typed = { catno: 'UPCJ-9085', barcode: '' }

  it('入れた時点で下書きに重なり、候補を選び直しても残る', () => {
    const s0 = looked(response([shokai, cand(true, 'B')]))
    const s1 = cdReducer(s0, { type: 'set_typed', typed })
    expect(s1.draft).toMatchObject({ catalog_number: 'UPCJ-9085' })
    const s2 = cdReducer(s1, { type: 'select', index: 0 })
    expect(s2.draft).toMatchObject({ catalog_number: 'UPCJ-9085', barcode: '' })
    const s3 = cdReducer(s2, { type: 'set_copy_scope', scope: 'minimal' })
    expect(s3.draft).toMatchObject({ catalog_number: 'UPCJ-9085', barcode: '' })
    const s4 = cdReducer(s3, { type: 'start_manual' })
    expect(s4.draft).toMatchObject({ catalog_number: 'UPCJ-9085' })
  })

  it('消すと候補の値に戻る', () => {
    const s0 = cdReducer(looked(response([shokai])), { type: 'select', index: 0 })
    const s1 = cdReducer(s0, { type: 'set_typed', typed })
    const s2 = cdReducer(s1, { type: 'set_typed', typed: { catno: '', barcode: '' } })
    expect(s2.draft).toMatchObject({ catalog_number: 'UPCJ-9001', barcode: '4988031278079' })
  })

  it('照会の結果にも重なる（品番で当たった候補が選ばれる）', () => {
    const s0 = cdReducer(looked(response([])), { type: 'set_typed', typed: { catno: 'upcj 9001', barcode: '' } })
    const hit: ReleaseCandidate = { ...shokai, exact: false, matched_by: ['catno'] }
    const s1 = run([{ type: 'lookup_start' }, { type: 'lookup_ok', result: response([hit, cand(true, 'B')]) }], s0)
    expect(s1.selected).toBe(0)
    expect(s1.draft).toMatchObject({ catalog_number: 'UPCJ-9001', barcode: '4988031278079' })
  })

  it('別のディスクに替わったら消す', () => {
    const s0 = run([{ type: 'set_disc', toc: '0:1000:5000', tracks }, { type: 'set_typed', typed }])
    expect(s0.draft).toMatchObject({ catalog_number: 'UPCJ-9085' })
    const s1 = cdReducer(s0, { type: 'set_disc', toc: '0:2000:6000', tracks })
    expect(s1.typed).toEqual({ catno: '', barcode: '' })
    expect(s1.draft).toMatchObject({ catalog_number: '' })
    // 同じディスクのポーリングでは消さない
    expect(cdReducer(s0, { type: 'set_disc', toc: '0:1000:5000', tracks }).typed).toEqual(typed)
  })
})
