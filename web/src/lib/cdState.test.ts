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
  })
})

describe('cdReducer: フォームの編集と貼り付け', () => {
  const base = looked(response([]))
  it('update_draft / update_track はフォームだけを変える', () => {
    let s = cdReducer(base, { type: 'update_draft', patch: { album: 'X' } })
    expect(s.draft!.album).toBe('X')
    s = cdReducer(s, { type: 'update_track', index: 1, patch: { title: 'two' } })
    // 空のままの行は `Track NN` を表でプレースホルダに出す（値は空。確定時に埋まる）
    expect(s.draft!.tracks.map((t) => t.title)).toEqual(['', 'two'])
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

describe('cdReducer: ディスクの出し入れ（P4-20）', () => {
  const filled = run(
    [{ type: 'update_draft', patch: { album: 'X', album_artist: 'Y' } }],
    looked(response([])),
  )
  it('ディスクが入ったら照会の前からフォームができる', () => {
    const s = cdReducer(initialCdState, { type: 'set_disc', toc: '0:1000:2500', tracks })
    expect(s.toc).toBe('0:1000:2500')
    expect(s.draft?.tracks).toHaveLength(2)
    expect(s.draft?.tracks[0]?.title).toBe('')
    expect(s.result).toBeNull()
  })
  it('同じディスクの set_disc は編集中の内容を消さない（2 秒ごとのポーリング）', () => {
    let s = cdReducer(initialCdState, { type: 'set_disc', toc: '0:1000:2500', tracks })
    s = cdReducer(s, { type: 'update_draft', patch: { album: '書きかけ' } })
    const again = cdReducer(s, { type: 'set_disc', toc: '0:1000:2500', tracks })
    expect(again).toBe(s)
  })
  it('別のディスクに替わるとフォームごと作り直す', () => {
    let s = cdReducer(initialCdState, { type: 'set_disc', toc: '0:1000:2500', tracks })
    s = cdReducer(s, { type: 'update_draft', patch: { album: '書きかけ' } })
    s = cdReducer(s, { type: 'set_disc', toc: '0:3000', tracks: [{ number: 1, length_ms: 40000 }] })
    expect(s.draft?.album).toBe('')
    expect(s.draft?.tracks).toHaveLength(1)
  })
  it('照会を始めても表は消えない', () => {
    const s = cdReducer(filled, { type: 'lookup_start' })
    expect(s.draft).toBe(filled.draft)
    expect(s.busy).toBe(true)
  })
  it('照会に失敗しても表と編集中の内容は残る', () => {
    let s = cdReducer(filled, { type: 'lookup_start' })
    s = cdReducer(s, { type: 'lookup_error', error: 'MusicBrainz に届かない' })
    expect(s.draft?.album).toBe('X')
    expect(s.error).toBe('MusicBrainz に届かない')
    expect(s.busy).toBe(false)
  })
  it('手で直したあとの再照会でも、返るまで編集中の内容は残る', () => {
    let s = cdReducer(filled, { type: 'update_track', index: 0, patch: { title: '手で入れた' } })
    s = cdReducer(s, { type: 'lookup_start' })
    expect(s.draft?.tracks[0]?.title).toBe('手で入れた')
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
  it('TOC を編集すると結果・フォーム・貼り付けの本文が消える（同じ TOC なら保つ）', () => {
    const s = { ...filled, paste: 'p' }
    const same = cdReducer(s, { type: 'set_toc', toc: s.toc })
    expect(same.draft).toBe(s.draft)
    const edited = cdReducer(s, { type: 'set_toc', toc: '0:2000' })
    expect(edited).toMatchObject({ toc: '0:2000', result: null, draft: null, selected: null, paste: '' })
  })
  it('照会のやり直しは貼り付けの本文を保つ（同じディスク）', () => {
    const s = run([{ type: 'lookup_start' }, { type: 'lookup_ok', result: response([]) }], { ...filled, paste: 'p' })
    expect(s.paste).toBe('p')
    // 候補ゼロ件なら、TOC から作ってあるフォームがそのまま残る
    expect(s.draft).toBe(filled.draft)
  })
})
