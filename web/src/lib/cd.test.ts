import { describe, expect, it } from 'vitest'
import {
  albumTags,
  applyTracklist,
  candidateLengthMs,
  candidateSummary,
  draftFromCandidate,
  emptyDraft,
  fillEmptyTitles,
  finalizeDraft,
  initialSelection,
  lookupHeadline,
  normalizeTocInput,
  outcomeAfterTocEdit,
  trackTags,
  validateDraft,
  type DiscDraft,
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
    const r = { discid: 'd', mb_toc: '', accuraterip_id: '', ctdb_toc_id: '', exact: true, candidates: [base], tracks: [] }
    expect(lookupHeadline(r)).toBe('DiscID が一致: 1 件')
    expect(lookupHeadline({ ...r, exact: false, candidates: [{ ...base, exact: false }] })).toBe(
      'TOC の近い候補（DiscID は未登録）: 1 件',
    )
    // fuzzy 経路でも候補側に DiscID 一致があれば「未登録」と言わない
    expect(lookupHeadline({ ...r, exact: false, candidates: [base, { ...base, exact: false }] })).toBe(
      'TOC で照会（DiscID の一致する候補 1 件を含む）: 2 件',
    )
    expect(lookupHeadline({ ...r, exact: false, candidates: [] })).toBe('MusicBrainz に見つからない（手入力へ）')
  })
})

describe('状態遷移', () => {
  const r = { discid: 'd', mb_toc: '', accuraterip_id: '', ctdb_toc_id: '', exact: true, candidates: [base], tracks: [] }
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

// ---------------------------------------------------------------- 手入力（P2-4）

const toc = [
  { number: 1, length_ms: 1000 },
  { number: 2, length_ms: 2500 },
  { number: 3, length_ms: 4000 },
]

describe('draftFromCandidate / emptyDraft', () => {
  it('行は TOC の音声トラック数。候補のトラックは位置で写し、足りない行は空', () => {
    const d = draftFromCandidate(base, toc)
    expect(d.source).toBe('musicbrainz')
    expect(d.release_id).toBe('r')
    expect(d.album).toBe('T')
    expect(d.album_artist).toBe('A')
    expect(d.date).toBe('1991-09-24')
    expect(d.label).toBe('DGC Records')
    expect(d.catalog_number).toBe('DGCD-24425')
    expect(d.barcode).toBe('720642442524')
    expect(d.disc_no).toBe(1)
    expect(d.disc_count).toBe(1)
    expect(d.tracks.map((t) => [t.number, t.title, t.artist, t.length_ms])).toEqual([
      [1, 'a', 'A', 1000],
      [2, 'b', 'A', 2500],
      [3, '', '', 4000],
    ])
    expect(d.tracks[0]!.mb).toEqual({ recording_id: 'x', track_id: 'y', isrcs: [] })
    expect(d.tracks[2]!.mb).toBeNull()
  })
  it('候補が TOC より多いトラックを持っていても TOC の行数に切る（警告は validate で出す）', () => {
    const d = draftFromCandidate(base, [toc[0]!])
    expect(d.tracks).toHaveLength(1)
  })
  it('空のフォーム: 手入力、アルバム欄は空、行は TOC から', () => {
    const d = emptyDraft(toc)
    expect(d.source).toBe('manual')
    expect(d.release_id).toBeNull()
    expect(d.album).toBe('')
    expect(d.disc_no).toBe(1)
    expect(d.disc_count).toBe(1)
    expect(d.tracks.map((t) => [t.number, t.title, t.artist, t.length_ms, t.mb])).toEqual([
      [1, '', '', 1000, null],
      [2, '', '', 2500, null],
      [3, '', '', 4000, null],
    ])
  })
})

describe('applyTracklist', () => {
  it('番号で行に写す。アーティストが無い行は既存を保つ', () => {
    const d: DiscDraft = { ...emptyDraft(toc), album_artist: 'AA' }
    d.tracks[1]!.artist = 'keep'
    const r = applyTracklist(d, [
      { no: 1, title: 'one', artist: 'X' },
      { no: 2, title: 'two', artist: null },
    ])
    expect(r.draft.tracks.map((t) => [t.title, t.artist])).toEqual([
      ['one', 'X'],
      ['two', 'keep'],
      ['', ''],
    ])
    expect(r.draft.album_artist).toBe('AA')
    expect(r.warnings).toEqual(['貼り付けの行数 2 が TOC の 3 と違う', '未設定の行: 3'])
    // 元は変えない
    expect(d.tracks[0]!.title).toBe('')
  })
  it('TOC に無い番号は捨てて警告', () => {
    const r = applyTracklist(emptyDraft(toc), [
      { no: 1, title: 'one', artist: null },
      { no: 2, title: 'two', artist: null },
      { no: 3, title: 'three', artist: null },
      { no: 4, title: 'four', artist: null },
    ])
    expect(r.draft.tracks.map((t) => t.title)).toEqual(['one', 'two', 'three'])
    expect(r.warnings).toEqual(['貼り付けの行数 4 が TOC の 3 と違う', 'TOC に無い番号: 4'])
  })
  it('全部そろえば警告なし', () => {
    const r = applyTracklist(emptyDraft(toc), [
      { no: 1, title: 'one', artist: null },
      { no: 2, title: 'two', artist: null },
      { no: 3, title: 'three', artist: null },
    ])
    expect(r.warnings).toEqual([])
  })
})

describe('validateDraft / fillEmptyTitles / finalizeDraft', () => {
  it('アルバム名・アルバムアーティスト・各トラック名が要る。日付は YYYY[-MM[-DD]]', () => {
    const d = emptyDraft(toc)
    expect(validateDraft(d)).toEqual(['アルバム名が空', 'アルバムアーティストが空', 'タイトルが空: 1, 2, 3'])
    const ok: DiscDraft = {
      ...d,
      album: 'X',
      album_artist: 'Y',
      tracks: d.tracks.map((t) => ({ ...t, title: 't' })),
    }
    expect(validateDraft(ok)).toEqual([])
    expect(validateDraft({ ...ok, date: '2024' })).toEqual([])
    expect(validateDraft({ ...ok, date: '2024-03' })).toEqual([])
    expect(validateDraft({ ...ok, date: '2024-03-09' })).toEqual([])
    expect(validateDraft({ ...ok, date: '2024/03/09' })).toEqual(['日付は YYYY / YYYY-MM / YYYY-MM-DD'])
    expect(validateDraft({ ...ok, album: '  ' })).toEqual(['アルバム名が空'])
    expect(validateDraft({ ...ok, disc_no: 3, disc_count: 2 })).toEqual(['ディスク番号 3 が枚数 2 を超える'])
  })
  it('空のタイトルを Track NN で埋める（入力済みは触らない）', () => {
    const d = emptyDraft(toc)
    d.tracks[1]!.title = 'two'
    expect(fillEmptyTitles(d).tracks.map((t) => t.title)).toEqual(['Track 01', 'two', 'Track 03'])
  })
  it('確定: 前後の空白を落とし、トラックのアーティストが空ならアルバムアーティスト', () => {
    const d: DiscDraft = {
      ...emptyDraft(toc),
      album: ' X ',
      album_artist: ' Y ',
      date: '',
      label: ' L ',
      tracks: emptyDraft(toc).tracks.map((t, i) => ({ ...t, title: ` t${i} `, artist: i === 0 ? ' Z ' : '' })),
    }
    const m = finalizeDraft(d)
    expect(m.album).toBe('X')
    expect(m.album_artist).toBe('Y')
    expect(m.date).toBeNull()
    expect(m.label).toBe('L')
    expect(m.catalog_number).toBeNull()
    expect(m.tracks.map((t) => [t.number, t.title, t.artist])).toEqual([
      [1, 't0', 'Z'],
      [2, 't1', 'Y'],
      [3, 't2', 'Y'],
    ])
    expect(m.source).toBe('manual')
    expect(m.release_id).toBeNull()
  })
  it('候補からの確定は MusicBrainz の ID を持ち越す', () => {
    const m = finalizeDraft(draftFromCandidate({ ...base, tracks: base.tracks.slice(0, 1) }, [toc[0]!]))
    expect(m.source).toBe('musicbrainz')
    expect(m.release_id).toBe('r')
    expect(m.tracks[0]!.mb).toEqual({ recording_id: 'x', track_id: 'y', isrcs: [] })
  })
})

describe('albumTags / trackTags', () => {
  it('MusicBrainz の id の写像: recording → MUSICBRAINZ_TRACKID、track → MUSICBRAINZ_RELEASETRACKID', () => {
    const m = finalizeDraft(
      draftFromCandidate({ ...base, tracks: [{ ...base.tracks[0]!, isrcs: ['USGF19942501', 'JPX'] }] }, [toc[0]!]),
    )
    // ISRC は多値のまま（Vorbis コメントは同じキーを反復する。`;` で繋がない）
    expect(trackTags(m.tracks[0]!)).toEqual([
      ['TRACKNUMBER', ['1']],
      ['TITLE', ['a']],
      ['ARTIST', ['A']],
      ['MUSICBRAINZ_TRACKID', ['x']],
      ['MUSICBRAINZ_RELEASETRACKID', ['y']],
      ['ISRC', ['USGF19942501', 'JPX']],
    ])
    expect(albumTags(m)).toEqual([
      ['ALBUM', ['T']],
      ['ALBUMARTIST', ['A']],
      ['DATE', ['1991-09-24']],
      ['LABEL', ['DGC Records']],
      ['CATALOGNUMBER', ['DGCD-24425']],
      ['BARCODE', ['720642442524']],
      ['DISCNUMBER', ['1']],
      ['DISCTOTAL', ['1']],
      ['MUSICBRAINZ_ALBUMID', ['r']],
    ])
  })
  it('手入力なら MusicBrainz の行は出ない。ISRC が空なら行ごと出ない', () => {
    const d = { ...emptyDraft([toc[0]!]), album: 'X', album_artist: 'Y' }
    d.tracks[0]!.title = 't'
    const m = finalizeDraft(d)
    expect(albumTags(m).map(([k]) => k)).toEqual(['ALBUM', 'ALBUMARTIST', 'DISCNUMBER', 'DISCTOTAL'])
    expect(trackTags(m.tracks[0]!)).toEqual([
      ['TRACKNUMBER', ['1']],
      ['TITLE', ['t']],
      ['ARTIST', ['Y']],
    ])
    expect(trackTags({ ...m.tracks[0]!, mb: { recording_id: 'x', track_id: 'y', isrcs: [] } }).map(([k]) => k)).toEqual([
      'TRACKNUMBER',
      'TITLE',
      'ARTIST',
      'MUSICBRAINZ_TRACKID',
      'MUSICBRAINZ_RELEASETRACKID',
    ])
  })
})

describe('category（配置先。D-67）', () => {
  it('空のフォームと候補の写しは category を持たない', () => {
    expect(emptyDraft(toc).category).toBeNull()
    expect(draftFromCandidate(base, toc).category).toBeNull()
  })
  it('確定で category をそのまま持ち越し、タグには写さない', () => {
    const d: DiscDraft = {
      ...emptyDraft(toc),
      album: 'X',
      album_artist: 'Y',
      category: 'J-Pop',
      tracks: emptyDraft(toc).tracks.map((t) => ({ ...t, title: 't' })),
    }
    const m = finalizeDraft(d)
    expect(m.category).toBe('J-Pop')
    expect(albumTags(m).some(([k]) => k === 'CATEGORY')).toBe(false)
    // 無ければ null（_Unsorted に置かれる）。空文字も null
    expect(finalizeDraft({ ...d, category: null }).category).toBeNull()
    expect(finalizeDraft({ ...d, category: '' }).category).toBeNull()
  })
})
