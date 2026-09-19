import { describe, expect, it } from 'vitest'
import {
  codecSummary,
  draftForSubmit,
  draftFrom,
  itemTitle,
  stateLabel,
  validateDraft,
  type InboxDraft,
  type InboxFile,
  type InboxItem,
} from './inbox'

function file(rel_path: string, codec = 'flac'): InboxFile {
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
    tags: [],
  }
}

const proposal: InboxDraft = {
  category: null,
  albumartist: 'A',
  album: 'X',
  date: '2020',
  tracks: [
    { rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: 'one', artist: '' },
    { rel_path: 'd/02.flac', disc_no: 1, track_no: 2, title: 'two', artist: '' },
  ],
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
    }
    const d = draftFrom(item({ draft: saved }))
    expect(d.category).toBe('Rock')
    expect(d.albumartist).toBe('B')
    expect(d.album).toBe('Y')
    expect(d.date).toBeNull()
    expect(d.tracks).toEqual([
      { rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: 'one', artist: '' },
      { rel_path: 'd/02.flac', disc_no: 2, track_no: 9, title: 'fixed', artist: 'C' },
    ])
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
    }
    expect(draftForSubmit(d)).toEqual({
      category: null,
      albumartist: 'A',
      album: 'X',
      date: null,
      tracks: [{ rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: 't', artist: '' }],
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
