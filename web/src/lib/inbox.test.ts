import { describe, expect, it } from 'vitest'
import {
  artistValues,
  artworkUrl,
  codecSummary,
  itemCover,
  keepArtistsFor,
  pictureOf,
  destinationLabel,
  draftForSubmit,
  draftFrom,
  itemTitle,
  parseUrlLines,
  stateLabel,
  validateDraft,
  verdictLabel,
  type InboxDraft,
  type InboxFile,
  type InboxItem,
} from './inbox'

function file(rel_path: string, codec = 'flac', tags: Array<[string, string]> = []): InboxFile {
  return {
    rel_path,
    inode: 1,
    size: 100,
    mtime_ns: 0,
    ctime_ns: 0,
    codec,
    lossless: codec !== 'mp3',
    sample_rate: 44100,
    bit_depth: 16,
    channels: 2,
    duration_ms: 1000,
    tags,
    source: null,
  }
}

const proposal: InboxDraft = {
  category: null,
  albumartist: 'A',
  album: 'X',
  date: '2020',
  tracks: [
    { rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: 'one', artist: '', keep_artists: false },
    { rel_path: 'd/02.flac', disc_no: 1, track_no: 2, title: 'two', artist: '', keep_artists: false },
  ],
  album_gain: false,
}

function item(over: Partial<InboxItem> = {}): InboxItem {
  return {
    id: 7,
    rel_dir: 'd',
    state: 'pending',
    detected_at: 0,
    seen_at: 0,
    approved_at: null,
    draft: null,
    error: null,
    placed_album_id: null,
    placed_at: null,
    tracks: [file('d/01.flac'), file('d/02.flac')],
    proposal,
    warnings: [],
    destination: null,
    ...over,
  }
}

describe('draftFrom', () => {
  it('保存済みの下書きが無ければ提案をそのまま使う', () => {
    const d = draftFrom(item())
    expect(d).toEqual(proposal)
    // 提案のオブジェクトを共有しない（フォームの編集が item に漏れない）
    expect(d).not.toBe(proposal)
    expect(d.tracks[0]).not.toBe(proposal.tracks[0])
  })

  it('保存済みの下書きはアルバム単位の値を優先し、トラックは現在のファイルに合わせる', () => {
    const saved: InboxDraft = {
      category: 'Rock',
      albumartist: 'B',
      album: 'Y',
      date: null,
      tracks: [
        // 02 は補正済み、gone は既に無いファイル、01 は下書きに無い（走査で増えた）
        { rel_path: 'd/02.flac', disc_no: 2, track_no: 9, title: 'fixed', artist: 'C' },
        { rel_path: 'd/gone.flac', disc_no: 1, track_no: 3, title: 'gone', artist: '' },
      ],
      album_gain: true,
    }
    const d = draftFrom(item({ draft: saved }))
    expect(d.album_gain).toBe(true)
    expect(d.category).toBe('Rock')
    expect(d.albumartist).toBe('B')
    expect(d.album).toBe('Y')
    expect(d.date).toBeNull()
    expect(d.tracks).toEqual([
      { rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: 'one', artist: '', keep_artists: false },
      { rel_path: 'd/02.flac', disc_no: 2, track_no: 9, title: 'fixed', artist: 'C', keep_artists: false },
    ])
  })

  it('album_gain は保存済みが無ければ追記先の現在値、追記先も無ければ false（D-74）', () => {
    expect(draftFrom(item()).album_gain).toBe(false)
    const dest = { album_id: 1, album: 'x', track_count: 2, max_track_no: 2, album_gain: true }
    expect(draftFrom(item({ destination: dest })).album_gain).toBe(true)
    expect(draftFrom(item({ destination: dest, draft: { ...proposal, album_gain: false } })).album_gain).toBe(false)
  })
})

describe('validateDraft', () => {
  const files = ['d/01.flac', 'd/02.flac']

  it('提案そのものは問題なし', () => {
    expect(validateDraft(proposal, files)).toEqual([])
  })

  it('空のアルバム / アルバムアーティスト / タイトル、0 の番号、重複を報告する', () => {
    const d: InboxDraft = {
      ...proposal,
      album: ' ',
      albumartist: '',
      tracks: [
        { rel_path: 'd/01.flac', disc_no: 1, track_no: 0, title: '', artist: '' },
        { rel_path: 'd/02.flac', disc_no: 1, track_no: 2, title: 'b', artist: '' },
      ],
    }
    expect(validateDraft(d, files)).toEqual([
      'アルバム名が空',
      'アルバムアーティストが空',
      'タイトルが空: d/01.flac',
      'トラック番号 / ディスク番号は 1 以上: d/01.flac',
    ])
    const dup: InboxDraft = {
      ...proposal,
      tracks: [
        { rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: 'a', artist: '' },
        { rel_path: 'd/02.flac', disc_no: 1, track_no: 1, title: 'b', artist: '' },
      ],
    }
    expect(validateDraft(dup, files)).toEqual(['番号が重複: disc 1 track 1'])
  })

  it('ファイルの過不足は大文字小文字を無視して照合する', () => {
    const d: InboxDraft = {
      ...proposal,
      tracks: [
        { rel_path: 'D/01.FLAC', disc_no: 1, track_no: 1, title: 'a', artist: '' },
        { rel_path: 'd/03.flac', disc_no: 1, track_no: 2, title: 'b', artist: '' },
      ],
    }
    expect(validateDraft(d, files)).toEqual(['件に無いファイル: d/03.flac', '下書きに無いファイル: d/02.flac'])
    // 全ファイルを含んだ上で同じファイルをもう 1 行足しても通さない
    const dup: InboxDraft = {
      ...proposal,
      tracks: [...proposal.tracks, { rel_path: 'D/01.FLAC', disc_no: 1, track_no: 3, title: 'c', artist: '' }],
    }
    expect(validateDraft(dup, files)).toEqual(['下書きに同じファイルが 2 回: D/01.FLAC'])
  })

  it('日付は YYYY / YYYY-MM / YYYY-MM-DD だけ。category は空文字を許さない', () => {
    expect(validateDraft({ ...proposal, date: '2020-13' }, files)).toEqual([
      '日付の形が不正: 2020-13（YYYY / YYYY-MM / YYYY-MM-DD）',
    ])
    expect(validateDraft({ ...proposal, date: '20200101' }, files)).toHaveLength(1)
    expect(validateDraft({ ...proposal, date: '2020-01-31' }, files)).toEqual([])
    expect(validateDraft({ ...proposal, date: null }, files)).toEqual([])
    expect(validateDraft({ ...proposal, category: ' ' }, files)).toEqual(['category が空'])
  })
})

describe('draftForSubmit', () => {
  it('前後の空白を落とし、空の date / category は null にする', () => {
    const d: InboxDraft = {
      category: ' ',
      albumartist: ' A ',
      album: 'X ',
      date: '',
      tracks: [{ rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: ' t ', artist: ' ' }],
      album_gain: true,
    }
    expect(draftForSubmit(d)).toEqual({
      category: null,
      albumartist: 'A',
      album: 'X',
      date: null,
      tracks: [{ rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: 't', artist: '', keep_artists: false }],
      album_gain: true,
    })
  })
})

describe('表示', () => {
  it('件名と状態の表示名', () => {
    expect(itemTitle(item())).toBe('d')
    expect(itemTitle(item({ rel_dir: '' }))).toBe('(Inbox 直下)')
    expect(stateLabel('pending')).toBe('未処理')
    expect(stateLabel('placed')).toBe('配置済み')
  })

  it('コーデックの集合', () => {
    expect(codecSummary([file('a', 'wav'), file('b', 'flac'), file('c', 'flac')])).toBe('flac / wav')
    expect(codecSummary([])).toBe('')
  })
})

describe('destinationLabel / verdictLabel（D-70）', () => {
  it('追記先があれば「既存の『…』（N 曲）に追加」', () => {
    expect(
      destinationLabel({ album_id: 7, album: 'Songs of A', track_count: 12, max_track_no: 12, album_gain: false }),
    ).toBe('宛先: 既存の『Songs of A』（12 曲）に追加。番号は 13 から')
    expect(destinationLabel({ album_id: 7, album: null, track_count: 0, max_track_no: 0, album_gain: false })).toBe(
      '宛先: 既存のアルバム（0 曲）に追加。番号は 1 から',
    )
    expect(destinationLabel(null)).toBeNull()
  })
  it('判定は ok なら「判定済み」、それ以外は reason 付きの「未判定」', () => {
    const src = (verdict: string) => ({ source: 'youtube', url: null, channel: null, verdict, message: null })
    expect(verdictLabel(src('ok'))).toEqual({ text: '判定済み', ok: true })
    expect(verdictLabel(src('unmatched'))).toEqual({ text: '未判定（unmatched）', ok: false })
    expect(verdictLabel(src('unknown_channel'))).toEqual({ text: '未判定（unknown_channel）', ok: false })
  })
})

describe('parseUrlLines（操作タブの YouTube）', () => {
  it('1 行 1 URL。空行と前後の空白を落とし、重複は 1 つにする', () => {
    expect(parseUrlLines(' https://youtu.be/a \n\nhttps://youtu.be/b\r\nhttps://youtu.be/a\n')).toEqual([
      'https://youtu.be/a',
      'https://youtu.be/b',
    ])
    expect(parseUrlLines('\n  \n')).toEqual([])
  })
})

// ---------------------------------------------------------------- 忠実表示（P4-4、D-70）

describe('ARTIST の多値', () => {
  const multi = file('d/01.flac', 'flac', [
    ['ARTIST', '花譜'],
    ['ARTIST', ' 理芽 '],
    ['ARTIST', ''],
    ['TITLE', 'x'],
  ])
  const single = file('d/02.flac', 'flac', [['ARTIST', '花譜']])

  it('artistValues は ARTIST の全値（trim、空は除く、出現順）', () => {
    expect(artistValues(multi)).toEqual(['花譜', '理芽'])
    expect(artistValues(single)).toEqual(['花譜'])
    expect(artistValues(file('d/03.flac'))).toEqual([])
  })

  it('keepArtistsFor: 多値でなければ常に false、多値なら提案は true、保存値は boolean を尊重、旧下書きは先頭値のまま', () => {
    expect(keepArtistsFor(single, null)).toBe(false)
    expect(keepArtistsFor(single, { rel_path: 'd/02.flac', disc_no: 1, track_no: 1, title: '', artist: '花譜', keep_artists: true })).toBe(false)
    expect(keepArtistsFor(undefined, null)).toBe(false)
    expect(keepArtistsFor(multi, null)).toBe(true)
    const t = { rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: '', artist: '花譜' }
    expect(keepArtistsFor(multi, { ...t, keep_artists: false })).toBe(false)
    expect(keepArtistsFor(multi, { ...t, keep_artists: true, artist: 'zzz' })).toBe(true)
    // 旧下書き（欄なし / null）: artist が先頭値のままなら保つ
    expect(keepArtistsFor(multi, t)).toBe(true)
    expect(keepArtistsFor(multi, { ...t, keep_artists: null })).toBe(true)
    expect(keepArtistsFor(multi, { ...t, artist: '花譜; 理芽' })).toBe(false)
  })

  it('draftFrom は提案の keep_artists をファイルから決め、保存値が true でもファイルが多値でなければ false に戻す', () => {
    const it0 = item({
      tracks: [multi, single],
      proposal: {
        ...proposal,
        tracks: [
          { rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: 'one', artist: '花譜; 理芽', keep_artists: true },
          { rel_path: 'd/02.flac', disc_no: 1, track_no: 2, title: 'two', artist: '花譜', keep_artists: false },
        ],
      },
    })
    expect(draftFrom(it0).tracks.map((t) => t.keep_artists)).toEqual([true, false])
    const saved: InboxDraft = {
      ...proposal,
      tracks: [
        { rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: 'one', artist: 'C', keep_artists: false },
        { rel_path: 'd/02.flac', disc_no: 1, track_no: 2, title: 'two', artist: 'x', keep_artists: true },
      ],
    }
    const d = draftFrom(item({ ...it0, draft: saved }))
    expect(d.tracks.map((t) => t.keep_artists)).toEqual([false, false])
    expect(d.tracks[0].artist).toBe('C')
  })

  it('draftForSubmit は keep_artists を boolean にする（旧下書きの null は false）', () => {
    const d: InboxDraft = {
      ...proposal,
      tracks: [
        { rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: 'a', artist: 'A; B', keep_artists: true },
        { rel_path: 'd/02.flac', disc_no: 1, track_no: 2, title: 'b', artist: 'A', keep_artists: null },
      ],
    }
    expect(draftForSubmit(d).tracks.map((t) => t.keep_artists)).toEqual([true, false])
  })
})

describe('埋め込み画像', () => {
  const h1 = 'a'.repeat(64)
  const h2 = 'b'.repeat(64)
  const withPic = (rel: string, ...hashes: string[]) =>
    file(rel, 'flac', hashes.map((h) => ['PICTURE', `image/jpeg:${h}`] as [string, string]))

  it('pictureOf は PICTURE の先頭の hash、無ければ null', () => {
    expect(pictureOf(withPic('d/01.flac', h1, h2))).toBe(h1)
    expect(pictureOf(file('d/01.flac'))).toBeNull()
    expect(pictureOf(file('d/01.flac', 'flac', [['PICTURE', 'image/jpeg:']]))).toBeNull()
  })

  it('itemCover は各ファイルの代表の最頻（同数なら先に現れたもの）、無ければ null', () => {
    expect(itemCover(item({ tracks: [withPic('a', h1), withPic('b', h2), withPic('c', h2)] }))).toBe(h2)
    expect(itemCover(item({ tracks: [withPic('a', h1), withPic('b', h2)] }))).toBe(h1)
    expect(itemCover(item({ tracks: [withPic('a', h1), file('b'), withPic('c', h1, h2)] }))).toBe(h1)
    expect(itemCover(item({ tracks: [file('a')] }))).toBeNull()
  })

  it('artworkUrl', () => {
    expect(artworkUrl(7, h1)).toBe(`/api/inbox/7/artwork/${h1}`)
  })
})

