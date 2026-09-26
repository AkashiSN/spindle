import { describe, expect, it } from 'vitest'
import {
  albumTags,
  applyTyped,
  catnoMismatch,
  normalizeCatno,
  typedLookupExtra,
  typedProblems,
  candidateLengthMs,
  candidateSummary,
  draftFromCandidate,
  emptyDraft,
  finalizeDraft,
  initialSelection,
  discidSubmissionUrl,
  formatLengthDiff,
  isCdMedium,
  lengthDiffMs,
  lookupHeadline,
  mediaSummary,
  formatsSummary,
  groupReleaseSummary,
  currentListing,
  type GroupRelease,
  offersDiscidSubmission,
  matchedByLabel,
  releaseUrl,
  splitByMedium,
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
  matched_by: ['discid'],
  media: [{ position: 1, format: 'CD', track_count: 2 }],
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
  it('日付・国・レーベル・バーコードを並べる（形式と枚数は mediaSummary が出す）', () => {
    expect(candidateSummary(base)).toBe('1991-09-24 · US · DGC Records DGCD-24425 · JAN/UPC 720642442524')
  })
  it('非公式と注記も出す（形式・枚数は入れない）', () => {
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
    ).toBe('Sub Pop · Bootleg · first press')
  })
})

describe('matchedByLabel / discidSubmissionUrl', () => {
  it('候補のバッジは経路を強い順に並べる', () => {
    expect(matchedByLabel(base)).toBe('DiscID 一致')
    expect(matchedByLabel({ ...base, matched_by: ['isrc', 'toc'] })).toBe('ISRC / TOC 近似')
    expect(matchedByLabel({ ...base, matched_by: ['release'] })).toBe('指定')
    expect(matchedByLabel({ ...base, matched_by: ['barcode'] })).toBe('バーコード')
    expect(matchedByLabel({ ...base, matched_by: [] })).toBe('')
  })
  it('登録の案内は DiscID が本当に未登録のときだけ（fuzzy に混ざった一致候補があれば出さない）', () => {
    const r = { discid: 'd', mb_toc: '', accuraterip_id: '', ctdb_toc_id: '', stage: 'discid' as const, can_widen: false, exact: false, candidates: [] as ReleaseCandidate[], notes: [], tracks: [] }
    const toc: ReleaseCandidate = { ...base, exact: false, matched_by: ['toc'] }
    expect(offersDiscidSubmission({ ...r, candidates: [toc] })).toBe(true)
    expect(offersDiscidSubmission({ ...r, candidates: [] })).toBe(true)
    expect(offersDiscidSubmission({ ...r, candidates: [toc, base] })).toBe(false)
    expect(offersDiscidSubmission({ ...r, exact: true, candidates: [base] })).toBe(false)
  })
  it('DiscID の登録 URL は libdiscid と同じ形（id・トラック数・TOC は + 区切り）', () => {
    expect(discidSubmissionUrl('Pmj4hPdkGckCxpSFFMoexmR6r1s-', '1 2 40440 150 20294')).toBe(
      'https://musicbrainz.org/cdtoc/attach?id=Pmj4hPdkGckCxpSFFMoexmR6r1s-&tracks=2&toc=1+2+40440+150+20294',
    )
  })
})

describe('candidateLengthMs / lookupHeadline', () => {
  it('長さの合計。不明があれば null', () => {
    expect(candidateLengthMs(base)).toBe(3500)
    expect(candidateLengthMs({ ...base, tracks: [{ ...base.tracks[0]!, length_ms: null }] })).toBeNull()
  })
  it('見出しは exact / 経路 / 0 件で変える', () => {
    const r = {
      discid: 'd',
      mb_toc: '',
      accuraterip_id: '',
      ctdb_toc_id: '',
      stage: 'discid' as const,
      can_widen: false,
      exact: true,
      candidates: [base],
      notes: [],
      tracks: [],
    }
    expect(lookupHeadline(r)).toBe('DiscID が一致: 1 件')
    const toc: ReleaseCandidate = { ...base, exact: false, matched_by: ['toc'] }
    expect(lookupHeadline({ ...r, exact: false, stage: 'toc', candidates: [toc] })).toBe(
      '候補: 1 件（TOC 近似。DiscID は未登録）',
    )
    // 経路は強い順に並べて全部出す（同じ候補が複数の経路で出ても 1 回）
    const isrc: ReleaseCandidate = { ...base, exact: false, matched_by: ['isrc', 'barcode'] }
    const given: ReleaseCandidate = { ...base, exact: false, matched_by: ['release', 'isrc'] }
    expect(lookupHeadline({ ...r, exact: false, stage: 'ids', candidates: [given, isrc] })).toBe(
      '候補: 2 件（指定 / ISRC / バーコード。DiscID は未登録）',
    )
    // CD 画面では入力できないので「手入力へ」とは言わない（P4-20 追記）
    expect(lookupHeadline({ ...r, exact: false, candidates: [] })).toBe(
      'MusicBrainz に見つからない（そのまま取り込んで Inbox で名前を入れる）',
    )
  })
  // DiscID が 200 でも候補 0 件なら下の段へ落ちる（D-64 追記 4）。そのとき exact は真のままなので、
  // 段だけを見て「未登録」と言うと嘘になる
  it('DiscID が登録済みのまま下の段へ落ちたら、未登録と言わない', () => {
    const r = {
      discid: 'd',
      mb_toc: '',
      accuraterip_id: '',
      ctdb_toc_id: '',
      exact: true,
      stage: 'ids' as const,
      can_widen: true,
      candidates: [{ ...base, exact: false, matched_by: ['isrc' as const] }],
      notes: [],
      tracks: [],
    }
    expect(lookupHeadline(r)).toBe('候補: 1 件（ISRC。DiscID は登録済みだが曲数の合う候補が無い）')
    expect(lookupHeadline({ ...r, stage: 'toc', can_widen: false })).toContain('DiscID は登録済み')
    expect(lookupHeadline(r)).not.toContain('未登録')
  })
})

describe('状態遷移', () => {
  const r = { discid: 'd', mb_toc: '', accuraterip_id: '', ctdb_toc_id: '', stage: 'discid' as const, can_widen: false, exact: true, candidates: [base], notes: [], tracks: [] }
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
    const d = draftFromCandidate(base, toc, 'full')
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
  it('minimal は識別用の最小限だけ写す（レーベル・カタログ番号・JAN は空、トラック行は番号と長さだけ。D-72、P4-2）', () => {
    const withGroup = { ...base, release_group_id: 'rg' }
    expect(draftFromCandidate(withGroup, toc, 'full').release_group_id).toBe('rg')
    const d = draftFromCandidate(withGroup, toc, 'minimal')
    expect(d).toMatchObject({
      source: 'musicbrainz',
      release_id: 'r',
      release_group_id: null,
      album: 'T',
      album_artist: 'A',
      date: '1991-09-24',
      label: '',
      catalog_number: '',
      barcode: '',
      disc_no: 1,
      disc_count: 1,
    })
    expect(d.tracks.map((t) => [t.number, t.title, t.artist, t.length_ms, t.mb])).toEqual([
      [1, '', '', 1000, null],
      [2, '', '', 2500, null],
      [3, '', '', 4000, null],
    ])
  })
  it('候補が TOC より多いトラックを持っていても TOC の行数に切る（警告は validate で出す）', () => {
    const d = draftFromCandidate(base, [toc[0]!], 'full')
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

describe('validateDraft / finalizeDraft', () => {
  it('名前は空でもよい（候補の無い盤を Inbox へ。D-67 追記）。日付は YYYY[-MM[-DD]]', () => {
    const d = emptyDraft(toc)
    // 空のタイトルも止めない（finalizeDraft が Track NN で埋める。P4-20）
    expect(validateDraft(d)).toEqual([])
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
    expect(validateDraft({ ...ok, album: '  ' })).toEqual([])
    expect(validateDraft({ ...ok, disc_no: 3, disc_count: 2 })).toEqual(['ディスク番号 3 が枚数 2 を超える'])
  })
  it('確定時に空のタイトルは Track NN で埋まる（入力済みは触らない）', () => {
    const d = emptyDraft(toc)
    d.tracks[1]!.title = 'two'
    const m = finalizeDraft({ ...d, album: 'X', album_artist: 'Y' })
    expect(m.tracks.map((t) => t.title)).toEqual(['Track 01', 'two', 'Track 03'])
    // 名前が空のままでも取り込みの入力になる
    const nameless = finalizeDraft(d)
    expect([nameless.album, nameless.album_artist]).toEqual(['', ''])
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
    const m = finalizeDraft(draftFromCandidate({ ...base, tracks: base.tracks.slice(0, 1) }, [toc[0]!], 'full'))
    expect(m.source).toBe('musicbrainz')
    expect(m.release_id).toBe('r')
    expect(m.tracks[0]!.mb).toEqual({ recording_id: 'x', track_id: 'y', isrcs: [] })
  })
})

describe('albumTags / trackTags', () => {
  it('MusicBrainz の id の写像: recording → MUSICBRAINZ_TRACKID、track → MUSICBRAINZ_RELEASETRACKID', () => {
    const m = finalizeDraft(
      draftFromCandidate({ ...base, tracks: [{ ...base.tracks[0]!, isrcs: ['USGF19942501', 'JPX'] }] }, [toc[0]!], 'full'),
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
    expect(draftFromCandidate(base, toc, 'full').category).toBeNull()
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

describe('候補の見分け（P2-3 の UI 改修）', () => {
  const five: ReleaseCandidate = {
    ...base,
    release_id: 'f1223d63-f359-457d-b935-fc27eb24a6de',
    exact: false,
    matched_by: ['isrc'],
    media: [
      { position: 1, format: 'CD', track_count: 2 },
      { position: 2, format: 'Blu-ray', track_count: 1 },
    ],
    medium_position: 1,
    medium_count: 2,
  }

  it('収録構成は全媒体を並べ、いま見ている枚を示す', () => {
    expect(mediaSummary(five)).toBe('CD + Blu-ray の 1 枚目')
    // 同じ形式が複数あれば枚数を残す（CD 2 枚 + Blu-ray と CD + Blu-ray は別の版）
    expect(
      mediaSummary({
        ...five,
        media: [
          { position: 1, format: 'CD', track_count: 2 },
          { position: 2, format: 'CD', track_count: 3 },
          { position: 3, format: 'Blu-ray', track_count: 1 },
        ],
        medium_count: 3,
      }),
    ).toBe('CD 2 枚 + Blu-ray の 1 枚目')
    expect(
      mediaSummary({
        ...five,
        media: [
          { position: 1, format: 'CD', track_count: 12 },
          { position: 2, format: 'CD', track_count: 10 },
        ],
      }),
    ).toBe('CD 2 枚組の 1 枚目')
    expect(mediaSummary({ ...five, media: [{ position: 1, format: 'CD', track_count: 2 }], medium_count: 1 })).toBe('CD')
    // 形式が無い medium は「不明」
    expect(mediaSummary({ ...five, media: [{ position: 1, format: null, track_count: 2 }], medium_count: 1 })).toBe(
      '形式不明',
    )
    // media が空（DiscID 照会の応答など）は今まで通り medium の形式だけ
    expect(mediaSummary({ ...five, media: [], medium_count: 1, format: 'CD' })).toBe('CD')
  })

  it('吸い出せるのは CD 系の medium だけ（MusicBrainz の形式名）', () => {
    for (const f of [
      'CD',
      'CD-R',
      '8cm CD',
      'Enhanced CD',
      'HDCD',
      'Copy Control CD',
      'SHM-CD',
      'Blu-spec CD',
      'HQCD',
      'DTS CD',
      'CD+G',
      'Hybrid SACD (CD layer)',
      'DualDisc (CD side)',
      // 注記の無い複合ディスクは CD 面を持つ形式（MusicBrainz の Release/Format）
      'Hybrid SACD',
      'DualDisc',
      'DVDplus (CD side)',
      'Mixed Mode CD',
    ]) {
      expect(isCdMedium({ ...five, format: f }), f).toBe(true)
    }
    for (const f of [
      'Digital Media',
      'Blu-ray',
      'DVD-Video',
      'DVD-Audio',
      'HD-DVD',
      'SACD',
      'SHM-SACD',
      'Hybrid SACD (SACD layer)',
      'VCD',
      'SVCD',
      'DualDisc (DVD-Video side)',
      'DualDisc (DVD-Audio side)',
      // 音声 CD として吸い出せない
      'Data CD',
      '12" Vinyl',
      'Cassette',
      'MiniDisc',
      'Reel-to-reel',
    ]) {
      expect(isCdMedium({ ...five, format: f }), f).toBe(false)
    }
    // 形式が分からないものは隠さない
    expect(isCdMedium({ ...five, format: null })).toBe(true)
  })

  it('CD 系とそれ以外に分ける（順序は保つ）', () => {
    const digital: ReleaseCandidate = { ...five, release_id: 'd', format: 'Digital Media' }
    const split = splitByMedium([five, digital, { ...five, release_id: 'c2' }])
    expect(split.cd.map((c) => c.release_id)).toEqual([five.release_id, 'c2'])
    expect(split.other.map((c) => c.release_id)).toEqual(['d'])
  })

  it('MusicBrainz のリリースへのリンク', () => {
    expect(releaseUrl(five)).toBe('https://musicbrainz.org/release/f1223d63-f359-457d-b935-fc27eb24a6de')
  })

  it('ディスクとの長さ差（候補に不明があれば null）', () => {
    const toc = [
      { number: 1, length_ms: 1000 },
      { number: 2, length_ms: 2000 },
    ]
    // base の 2 曲は 1000 + 2500 = 3500 ms
    expect(lengthDiffMs(base, toc)).toBe(500)
    expect(formatLengthDiff(500)).toBe('長さ差 +0.5 秒')
    expect(formatLengthDiff(-1400)).toBe('長さ差 −1.4 秒')
    expect(formatLengthDiff(0)).toBe('長さ一致')
    expect(lengthDiffMs({ ...base, tracks: [{ ...base.tracks[0]!, length_ms: null }] }, toc)).toBeNull()
    expect(lengthDiffMs(base, [])).toBeNull()
  })
})

describe('リリースグループの版（D-93）', () => {
  it('formatsSummary は形式ごとの枚数を出てきた順に並べる', () => {
    expect(formatsSummary(['CD'])).toBe('CD')
    expect(formatsSummary(['CD', 'Blu-ray'])).toBe('CD + Blu-ray')
    expect(formatsSummary(['CD', 'CD'])).toBe('CD 2 枚組')
    expect(formatsSummary(['CD', 'CD', 'DVD-Video'])).toBe('CD 2 枚 + DVD-Video')
    expect(formatsSummary([null])).toBe('形式不明')
    expect(formatsSummary([])).toBe('形式不明')
  })

  const bd: GroupRelease = {
    release_id: 'f1223d63-f359-457d-b935-fc27eb24a6de',
    title: 'Five',
    disambiguation: null,
    date: '2026-05-31',
    country: 'JP',
    status: 'Official',
    formats: ['CD', 'Blu-ray'],
    label: 'Storm Labels',
    catalog_number: 'LCNC-0097 / LCNC-0098',
    front: true,
  }

  it('groupReleaseSummary は日付・国・形式・レーベルを並べ、Official は出さない', () => {
    expect(groupReleaseSummary(bd)).toBe('2026-05-31 · JP · CD + Blu-ray · Storm Labels LCNC-0097 / LCNC-0098')
    expect(
      groupReleaseSummary({
        ...bd,
        date: null,
        country: 'XW',
        formats: ['Digital Media'],
        catalog_number: null,
        status: 'Promotion',
        disambiguation: '初回限定盤',
      }),
    ).toBe('XW · Digital Media · Storm Labels · Promotion · 初回限定盤')
  })

  it('currentListing は表示中のグループの結果だけを返し、グループが変われば loading に戻す', () => {
    const value = { releases: [bd], total: 1 }
    const stored = { groupId: 'A', listing: { state: 'ok' as const, value } }
    expect(currentListing(null, 'A')).toEqual({ state: 'loading' })
    expect(currentListing(stored, 'A')).toEqual({ state: 'ok', value })
    // 候補を引き直してグループが B になった: A の版は出さない（選べない）
    expect(currentListing(stored, 'B')).toEqual({ state: 'loading' })
    // A の遅い応答が B の後に来ても、B の表示には使わない
    expect(currentListing(stored, 'B').state).toBe('loading')
    const failed = { groupId: 'B', listing: { state: 'error' as const, message: 'x' } }
    expect(currentListing(failed, 'B')).toEqual({ state: 'error', message: 'x' })
    expect(currentListing(failed, null)).toEqual({ state: 'loading' })
  })
})

describe('手入力の品番と JAN（D-94）', () => {
  const toc = [
    { number: 1, length_ms: 1000 },
    { number: 2, length_ms: 2500 },
  ]
  const shokai: ReleaseCandidate = {
    ...base,
    release_id: 'shokai',
    labels: [['Universal', 'UPCJ-9001']],
    barcode: '4988031278079',
    disambiguation: '初回限定盤A',
  }
  const typed = (catno: string, barcode = '') => ({ catno, barcode })

  it('品番はサーバと同じ規則で比べる形にする', () => {
    expect(normalizeCatno('UPCJ-9001')).toBe('UPCJ9001')
    expect(normalizeCatno(' upcj 9001 ')).toBe('UPCJ9001')
    expect(normalizeCatno('UPCJ*')).toBeNull()
    expect(normalizeCatno('ＵＰＣＪ－９００１')).toBeNull()
    expect(normalizeCatno(' - ')).toBeNull()
  })

  it('照会に載せられない形を知らせる。空は問題なし', () => {
    expect(typedProblems(typed(''))).toEqual([])
    expect(typedProblems(typed('UPCJ-9001', '4988031278079'))).toEqual([])
    expect(typedProblems(typed('UPCJ-9001', '49880312'))).toEqual([])
    expect(typedProblems(typed('UPCJ"9001'))).toHaveLength(1)
    expect(typedProblems(typed('A'.repeat(33)))).toHaveLength(1)
    expect(typedProblems(typed('', '12345'))).toHaveLength(1)
    expect(typedLookupExtra(typed(' UPCJ-9001 ', ''))).toEqual({ catno: 'UPCJ-9001', barcode: null })
  })

  it('候補の品番と一致すれば候補の表記と JAN を使う', () => {
    const d = applyTyped(draftFromCandidate(shokai, toc, 'full'), shokai, typed('upcj 9001'), 'full')
    expect(d.catalog_number).toBe('UPCJ-9001')
    expect(d.barcode).toBe('4988031278079')
    expect(catnoMismatch(shokai, typed('upcj 9001'))).toBeNull()
  })

  it('一致するのが 2 つ目のレーベルなら、そのレーベルと品番を写す', () => {
    const two: ReleaseCandidate = { ...shokai, labels: [['A', 'AAA-1'], ['B', 'UPCJ-9001']] }
    const d = applyTyped(draftFromCandidate(two, toc, 'full'), two, typed('UPCJ-9001'), 'full')
    expect(d).toMatchObject({ label: 'B', catalog_number: 'UPCJ-9001' })
  })

  it('食い違えば品番は入力値、JAN は写さない。ALBUMID などの id は残す', () => {
    const d = applyTyped(draftFromCandidate(shokai, toc, 'full'), shokai, typed('UPCJ-9085'), 'full')
    expect(d.catalog_number).toBe('UPCJ-9085')
    expect(d.barcode).toBe('')
    expect(d.release_id).toBe('shokai')
    expect(d.label).toBe('Universal')
    expect(d.tracks[0]!.mb).not.toBeNull()
    const m = catnoMismatch(shokai, typed('UPCJ-9085'))
    expect(m).toContain('UPCJ-9001')
    expect(m).toContain('初回限定盤A')
    expect(m).toContain('UPCJ-9085')
  })

  it('食い違っても JAN を入れていればその値', () => {
    const d = applyTyped(draftFromCandidate(shokai, toc, 'full'), shokai, typed('UPCJ-9085', '4988031278086'), 'full')
    expect(d).toMatchObject({ catalog_number: 'UPCJ-9085', barcode: '4988031278086' })
  })

  it('写す範囲が最小限でも入力値は書く', () => {
    const d = applyTyped(draftFromCandidate(shokai, toc, 'minimal'), shokai, typed('UPCJ-9001', '4988031278079'), 'minimal')
    expect(d).toMatchObject({ catalog_number: 'UPCJ-9001', barcode: '4988031278079', label: '' })
  })

  it('候補を使わないときも入力値を書く。未入力なら下書きはそのまま', () => {
    const empty = emptyDraft(toc)
    expect(applyTyped(empty, null, typed('UPCJ-9085', '4988031278086'), 'full')).toMatchObject({
      catalog_number: 'UPCJ-9085',
      barcode: '4988031278086',
    })
    const d = draftFromCandidate(shokai, toc, 'full')
    expect(applyTyped(d, shokai, typed(''), 'full')).toBe(d)
    expect(catnoMismatch(null, typed('UPCJ-9085'))).toBeNull()
  })

  it('品番で当たった候補が 1 件なら、DiscID の候補より先にそれを選ぶ', () => {
    const r = {
      discid: 'd',
      mb_toc: '',
      accuraterip_id: '',
      ctdb_toc_id: '',
      exact: true,
      stage: 'discid' as const,
      can_widen: false,
      notes: [],
      tracks: toc,
      candidates: [
        { ...base, release_id: 'normal', exact: false, matched_by: ['catno' as const] },
        { ...shokai, matched_by: ['discid' as const] },
      ],
    }
    expect(initialSelection(r)).toBe(0)
    expect(matchedByLabel(r.candidates[0]!)).toBe('品番一致')
  })
})
