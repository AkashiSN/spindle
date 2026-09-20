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
  exact,
  candidates,
  tracks,
})

function run(actions: CdAction[], from: CdState = initialCdState): CdState {
  return actions.reduce(cdReducer, from)
}

const looked = (r: LookupResponse) => run([{ type: 'set_toc', toc: '0:1000:5000' }, { type: 'lookup_start' }, { type: 'lookup_ok', result: r }])

describe('cdReducer: 照会', () => {
  it('lookup_start で busy、結果と下の段を消す', () => {
    const s = cdReducer({ ...initialCdState, result: response([]), draft: null, error: 'e' }, { type: 'lookup_start' })
    expect(s).toMatchObject({ busy: true, error: null, result: null, draft: null, confirmed: null })
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
  it('exact が複数、または近似だけなら選ばず、フォームも出さない', () => {
    expect(looked(response([cand(true), cand(true)])).draft).toBeNull()
    expect(looked(response([cand(false)])).draft).toBeNull()
  })
  it('lookup_error は結果を消してエラーを出す', () => {
    const s = run([{ type: 'lookup_start' }, { type: 'lookup_error', error: 'x' }])
    expect(s).toMatchObject({ busy: false, error: 'x', result: null, draft: null })
  })
})

describe('cdReducer: 選択と手入力', () => {
  const two = response([cand(false, 'X'), cand(false, 'Y')])
  it('select で候補をフォームに写す。編集中の内容は捨てる', () => {
    let s = run([{ type: 'select', index: 0 }, { type: 'update_draft', patch: { album: 'edited' } }], looked(two))
    expect(s.draft).toMatchObject({ album: 'edited', source: 'musicbrainz' })
    s = cdReducer(s, { type: 'select', index: 1 })
    expect(s.selected).toBe(1)
    expect(s.draft).toMatchObject({ album: 'Y' })
  })
  it('start_manual で空のフォーム、selected は外れる', () => {
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
  it('既定は minimal: exact の自動選択も select も最小限だけ写す', () => {
    const s = looked(two)
    expect(s.copyScope).toBe('minimal')
    expect(s.selected).toBe(0)
    expect(s.draft).toMatchObject({ album: 'X', label: '', catalog_number: '', barcode: '' })
    expect(s.draft!.tracks.every((t) => t.title === '' && t.artist === '' && t.mb == null)).toBe(true)
    const t = cdReducer(s, { type: 'select', index: 1 })
    expect(t.draft).toMatchObject({ album: 'Y', label: '' })
    expect(t.draft!.tracks.every((t) => t.title === '')).toBe(true)
  })
  it('set_copy_scope: full で選択中の候補を写し直し、minimal に戻すと空行になる（編集中の内容は捨てる）', () => {
    let s = run([{ type: 'update_draft', patch: { album: 'edited' } }], looked(two))
    s = cdReducer(s, { type: 'set_copy_scope', scope: 'full' })
    expect(s.copyScope).toBe('full')
    expect(s.selected).toBe(0)
    expect(s.draft!.album).toBe('X')
    expect(s.draft!.tracks[0]).toMatchObject({ title: 'a', artist: 'A', mb: { recording_id: 'x', track_id: 'y', isrcs: [] } })
    s = cdReducer(s, { type: 'set_copy_scope', scope: 'minimal' })
    expect(s.draft!.tracks[0]).toMatchObject({ title: '', artist: '', mb: null })
  })
  it('同じ範囲なら何もしない。手入力中は範囲だけ変わる。確定後も範囲だけ変わりフォームは残る', () => {
    const s = looked(two)
    expect(cdReducer(s, { type: 'set_copy_scope', scope: 'minimal' })).toBe(s)
    const manual = cdReducer(s, { type: 'start_manual' })
    const m2 = cdReducer(manual, { type: 'set_copy_scope', scope: 'full' })
    expect(m2.copyScope).toBe('full')
    expect(m2.draft).toBe(manual.draft)
    const confirmed = run(
      [
        { type: 'set_copy_scope', scope: 'full' },
        { type: 'update_draft', patch: { album: 'ok', album_artist: 'aa' } },
        { type: 'fill_titles' },
        { type: 'confirm' },
      ],
      s,
    )
    expect(confirmed.confirmed).not.toBeNull()
    const c2 = cdReducer(confirmed, { type: 'set_copy_scope', scope: 'minimal' })
    expect(c2.copyScope).toBe('minimal')
    expect(c2.draft).toBe(confirmed.draft)
    expect(c2.confirmed).toBe(confirmed.confirmed)
  })
})

describe('cdReducer: フォームの編集と貼り付け', () => {
  const base = looked(response([]))
  it('update_draft / update_track / fill_titles はエラー表示を消す', () => {
    let s = cdReducer(base, { type: 'confirm' })
    expect(s.draftErrors.length).toBeGreaterThan(0)
    s = cdReducer(s, { type: 'update_draft', patch: { album: 'X' } })
    expect(s.draftErrors).toEqual([])
    expect(s.draft!.album).toBe('X')
    s = cdReducer(s, { type: 'update_track', index: 1, patch: { title: 'two' } })
    expect(s.draft!.tracks.map((t) => t.title)).toEqual(['', 'two'])
    s = cdReducer(s, { type: 'fill_titles' })
    expect(s.draft!.tracks.map((t) => t.title)).toEqual(['Track 01', 'two'])
  })
  it('apply_paste は貼り付けを行に写し、警告を残す', () => {
    const s = run(
      [
        { type: 'set_paste', text: '1. one / X\n2. two\n3. three' },
        { type: 'apply_paste' },
      ],
      base,
    )
    expect(s.draft!.tracks.map((t) => [t.title, t.artist])).toEqual([
      ['one', 'X'],
      ['two', ''],
    ])
    expect(s.pasteWarnings).toEqual(['貼り付けの行数 3 が TOC の 2 と違う', 'TOC に無い番号: 3'])
  })
  it('読めない貼り付けは警告だけでフォームは変えない', () => {
    const s = run([{ type: 'set_paste', text: '\n\n' }, { type: 'apply_paste' }], base)
    expect(s.draft).toEqual(base.draft)
    expect(s.pasteWarnings).toEqual(['貼り付けからトラックを読めない'])
  })
  it('artistFirst の向きは apply_paste に効く', () => {
    const s = run(
      [{ type: 'set_paste_artist_first', value: true }, { type: 'set_paste', text: '1. X / one' }, { type: 'apply_paste' }],
      base,
    )
    expect(s.draft!.tracks[0]).toMatchObject({ title: 'one', artist: 'X' })
  })
})

describe('cdReducer: 確定と巻き戻し', () => {
  const filled = run(
    [
      { type: 'update_draft', patch: { album: 'X', album_artist: 'Y' } },
      { type: 'fill_titles' },
    ],
    looked(response([])),
  )
  it('confirm は検証に通れば confirmed、通らなければ draftErrors', () => {
    const ok = cdReducer(filled, { type: 'confirm' })
    expect(ok.draftErrors).toEqual([])
    expect(ok.confirmed).toMatchObject({ album: 'X', album_artist: 'Y', source: 'manual' })
    expect(ok.confirmed!.tracks[1]).toMatchObject({ number: 2, title: 'Track 02', artist: 'Y' })
    const ng = cdReducer(looked(response([])), { type: 'confirm' })
    expect(ng.confirmed).toBeNull()
    expect(ng.draftErrors).toEqual(['アルバム名が空', 'アルバムアーティストが空', 'タイトルが空: 1, 2'])
  })
  it('確定後は select / start_manual を受け付けない。unconfirm でフォームに戻る（内容は保つ）', () => {
    const ok = cdReducer(filled, { type: 'confirm' })
    expect(cdReducer(ok, { type: 'start_manual' })).toBe(ok)
    const back = cdReducer(ok, { type: 'unconfirm' })
    expect(back.confirmed).toBeNull()
    expect(back.draft).toEqual(ok.draft)
  })
  it('TOC を編集すると結果・フォーム・確定・貼り付けの本文が消える（同じ TOC なら保つ）', () => {
    const ok = cdReducer({ ...filled, paste: 'p' }, { type: 'confirm' })
    const same = cdReducer(ok, { type: 'set_toc', toc: ok.toc })
    expect(same.confirmed).toEqual(ok.confirmed)
    const edited = cdReducer(ok, { type: 'set_toc', toc: '0:2000' })
    expect(edited).toMatchObject({ toc: '0:2000', result: null, draft: null, confirmed: null, selected: null, paste: '' })
  })
  it('reset も結果より下を全部消す（TOC は残す）', () => {
    const ok = cdReducer({ ...filled, paste: 'p' }, { type: 'confirm' })
    const s = cdReducer(ok, { type: 'reset' })
    expect(s).toMatchObject({ toc: ok.toc, result: null, draft: null, confirmed: null, paste: '', error: null })
  })
  it('照会のやり直しは貼り付けの本文を保つ（同じディスク）', () => {
    const s = run([{ type: 'lookup_start' }, { type: 'lookup_ok', result: response([]) }], { ...filled, paste: 'p' })
    expect(s.paste).toBe('p')
    expect(s.draft).toMatchObject({ album: '' })
  })
})
