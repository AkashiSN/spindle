import { describe, expect, it } from 'vitest'
import {
  applyPicture,
  draftChangeCount,
  effectiveTag,
  isLockedTagKey,
  newTagKeyProblem,
  pictureState,
  resetPictures,
  setTrackTag,
  tagChanged,
  trackPictureUrl,
  usesCaaPicture,
  itemThumbUrl,
  refreshedDraft,
  stickyColumns,
  extraTagKeys,
  inboxColumns,
  parseHiddenColumns,
  tagValue,
  applyCandidate,
  applyTracklist,
  artistValues,
  discNumbers,
  artworkUrl,
  codecSummary,
  itemCover,
  keepArtistsFor,
  pictureOf,
  destinationLabel,
  destinationText,
  discardDeadline,
  discardLabel,
  draftForSubmit,
  draftFrom,
  itemTitle,
  stateLabel,
  validateDraft,
  verdictLabel,
  sameTitleCount,
  sameTitleLabel,
  syncNote,
  watchLabel,
  type DraftTrack,
  type InboxDraft,
  type InboxFile,
  type InboxItem,
  omitsDisc,
} from './inbox'
import type { ReleaseCandidate } from './cd'

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

  it('album_gain は保存済みが無ければ追記先の現在値、追記先も無ければ提案の値（D-74）', () => {
    expect(draftFrom(item()).album_gain).toBe(false)
    const dest = { album_id: 1, album: 'x', track_count: 2, max_track_no: 2, album_gain: true }
    expect(draftFrom(item({ destination: dest })).album_gain).toBe(true)
    expect(draftFrom(item({ destination: dest, draft: { ...proposal, album_gain: false } })).album_gain).toBe(false)
    // CD の吸い出しはサーバが on で提案する
    const cd = item({ proposal: { ...proposal, album_gain: true } })
    expect(draftFrom(cd).album_gain).toBe(true)
    expect(draftFrom({ ...cd, destination: { ...dest, album_gain: false } }).album_gain).toBe(false)
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
    expect(validateDraft(d, files)).toEqual(['取り込みに無いファイル: d/03.flac', '下書きに無いファイル: d/02.flac'])
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
      release_id: null,
      release_group_id: null,
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

  it('削除待ちの期限と表示（D-90）', () => {
    const fmt = (t: number) => `t=${t}`
    const waiting = { state: 'rejected' as const, discard_requested_at: 1000 }
    expect(discardDeadline(waiting, 30)).toBe(1000 + 30 * 86_400)
    expect(discardLabel(waiting, 30, fmt)).toBe(
      `削除待ち: t=${1000 + 30 * 86_400} 以降の GC でファイルを消す（それまでは取り消せる）`,
    )
    // 日数が分からない（旧サーバ）ときは期限を出さない
    expect(discardDeadline(waiting, null)).toBeNull()
    expect(discardLabel(waiting, null, fmt)).toBe('削除待ち（GC がファイルを消す。それまでは取り消せる）')
    // 破棄待ちでない・rejected 以外は null
    expect(discardLabel({ state: 'rejected', discard_requested_at: null }, 30, fmt)).toBeNull()
    expect(discardLabel({ state: 'rejected' }, 30, fmt)).toBeNull()
    expect(discardDeadline({ state: 'pending', discard_requested_at: 1000 }, 30)).toBeNull()
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
  it('購読の件は再生リストの位置に入る番号を出し、宛先の番号と重なれば警告する（P4-22）', () => {
    const dest = { album_id: 713, album: '明透のお歌', track_count: 178, max_track_no: 179, album_gain: false,
                   numbers: [[1, 164], [1, 166]] as Array<[number, number]> }
    const sub = { source: 'youtube', url: null, channel: null, verdict: 'ok', message: null, subscription_id: 7, position: 165 }
    const it = item({ destination: dest, tracks: [{ ...file('y/165.opus', 'opus'), source: sub }] })
    const draft = (no: number) => ({ tracks: [{ rel_path: 'y/165.opus', disc_no: 1, track_no: no }] }) as InboxDraft
    expect(destinationText(it, draft(165))).toEqual({
      label: '宛先: 既存の『明透のお歌』（178 曲）に追加。番号 165 に入る（再生リストの位置。空けてある番号）',
      overlap: null,
    })
    expect(destinationText(it, draft(166))?.overlap).toBe('番号 166 は宛先に既にある（③ で直す）')
    // 購読でない件は従来の「max + 1 から」
    const plain = item({ destination: dest, tracks: [file('y/165.opus', 'opus')] })
    expect(destinationText(plain, draft(180))).toEqual({
      label: '宛先: 既存の『明透のお歌』（178 曲）に追加。番号は 180 から',
      overlap: null,
    })
    // 連続する番号は範囲にまとめる
    const two = item({
      destination: dest,
      tracks: [{ ...file('y/a.opus', 'opus'), source: sub }, { ...file('y/b.opus', 'opus'), source: sub }],
    })
    const both = { tracks: [
      { rel_path: 'y/a.opus', disc_no: 1, track_no: 170 },
      { rel_path: 'y/b.opus', disc_no: 1, track_no: 171 },
    ] } as InboxDraft
    expect(destinationText(two, both)?.label).toContain('番号 170〜171 に入る')
    expect(destinationText(item({ destination: null }), draft(1))).toBeNull()
  })

  it('ディスク番号の無い album への追記は DISCNUMBER を書かない（omitsDisc）', () => {
    const base = { album_id: 713, album: '明透のお歌', track_count: 178, max_track_no: 179, album_gain: false }
    const one = { tracks: [{ rel_path: 'y/165.opus', disc_no: 1, track_no: 165 }] } as InboxDraft
    const f = file('y/165.opus', 'opus')
    const noDisc = item({ destination: { ...base, uses_disc: false }, tracks: [f] })
    expect(omitsDisc(noDisc, one)).toBe(true)
    expect(destinationText(noDisc, one)?.label).toBe(
      '宛先: 既存の『明透のお歌』（178 曲）に追加。番号は 180 から。ディスク番号は付けない（宛先の曲に無い）',
    )
    // 宛先がディスク番号を使っている / 旧サーバ（uses_disc 無し）/ 宛先なし / CD の件 / 2 枚分は従来どおり
    expect(omitsDisc(item({ destination: { ...base, uses_disc: true }, tracks: [f] }), one)).toBe(false)
    expect(omitsDisc(item({ destination: base, tracks: [f] }), one)).toBe(false)
    expect(omitsDisc(item({ destination: null, tracks: [f] }), one)).toBe(false)
    const rip = { toc: '0:1:2', isrcs: [], mcn: null } as unknown as InboxItem['rip']
    expect(omitsDisc(item({ destination: { ...base, uses_disc: false }, tracks: [f], rip }), one)).toBe(false)
    const twoDiscs = { tracks: [
      { rel_path: 'a', disc_no: 1, track_no: 1 },
      { rel_path: 'b', disc_no: 2, track_no: 1 },
    ] } as InboxDraft
    expect(omitsDisc(item({ destination: { ...base, uses_disc: false }, tracks: [f] }), twoDiscs)).toBe(false)
    // 全トラックを disc 2 に直した取り込みはサーバと同じく書く（最大が 1 のときだけ省く）
    const allTwo = { tracks: [{ rel_path: 'y/165.opus', disc_no: 2, track_no: 165 }] } as InboxDraft
    expect(omitsDisc(noDisc, allTwo)).toBe(false)
  })

  it('同期が番号を空けた経緯（P4-22）', () => {
    const sub = { source: 'youtube', url: null, channel: null, verdict: 'ok', message: null, subscription_id: 7, position: 165 }
    const it = item({ tracks: [{ ...file('y/165.opus', 'opus'), source: sub }] })
    const draft = { tracks: [{ rel_path: 'y/165.opus', disc_no: 1, track_no: 165 }] } as InboxDraft
    const t = (e: number) => `T${e}`
    const s = { id: 7, album: '明透のお歌', synced_at: 5, moved: 14, renamed: 14, tags_batch_id: 14, rename_batch_id: 15 }
    expect(syncNote(it, draft, [s], t)).toBe(
      '同期（T5）で既存の 14 曲の番号を揃え、14 曲のファイル名を直して 165 を空けた（バッチ #14 / #15。履歴で巻き戻せる）。承認しても既存の曲は動かない',
    )
    expect(syncNote(it, draft, [{ ...s, moved: 0, renamed: 0, tags_batch_id: null, rename_batch_id: null }], t)).toBe(
      '直近の同期（T5）では番号の揃え直しは無かった。承認しても既存の曲の番号は動かない',
    )
    expect(syncNote(it, draft, [], t)).toBe('購読 #7 の同期の記録が無い')
    // 購読の件でなければ null
    expect(syncNote(item(), draft, [s], t)).toBeNull()
  })

  it('判定は ok なら「判定済み」、それ以外は reason 付きの「未判定」', () => {
    const src = (verdict: string) => ({ source: 'youtube', url: null, channel: null, verdict, message: null })
    expect(verdictLabel(src('ok'))).toEqual({ text: '判定済み', ok: true })
    expect(verdictLabel(src('unmatched'))).toEqual({ text: '未判定（unmatched）', ok: false })
    expect(verdictLabel(src('unknown_channel'))).toEqual({ text: '未判定（unknown_channel）', ok: false })
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
  const track = (over: Partial<DraftTrack> = {}) => ({
    rel_path: 'd/01.flac',
    disc_no: 1,
    track_no: 1,
    title: '',
    artist: '花譜',
    ...over,
  })

  it('artistValues は ARTIST の全値をそのまま（空白・空値も 1 値として数える。サーバと同じ）', () => {
    expect(artistValues(multi)).toEqual(['花譜', ' 理芽 ', ''])
    expect(artistValues(single)).toEqual(['花譜'])
    expect(artistValues(file('d/03.flac'))).toEqual([])
    // ["A", ""] も多値（1 値扱いにして配置で潰さない）
    expect(keepArtistsFor(file('d/04.flac', 'flac', [['ARTIST', 'A'], ['ARTIST', '']]), null, 'AA')).toBe(true)
  })

  it('keepArtistsFor: 多値でなければ常に false、多値なら提案は true、保存値は boolean を尊重、旧下書きは先頭値のまま', () => {
    expect(keepArtistsFor(single, null, 'AA')).toBe(false)
    expect(keepArtistsFor(single, track({ rel_path: 'd/02.flac', keep_artists: true }), 'AA')).toBe(false)
    expect(keepArtistsFor(undefined, null, 'AA')).toBe(false)
    expect(keepArtistsFor(multi, null, 'AA')).toBe(true)
    expect(keepArtistsFor(multi, track({ keep_artists: false }), 'AA')).toBe(false)
    expect(keepArtistsFor(multi, track({ keep_artists: true, artist: 'zzz' }), 'AA')).toBe(true)
    // 旧下書き（欄なし / null）: 実効アーティスト（空ならアルバムアーティスト）が先頭値のままなら保つ
    expect(keepArtistsFor(multi, track(), 'AA')).toBe(true)
    expect(keepArtistsFor(multi, track({ keep_artists: null }), 'AA')).toBe(true)
    expect(keepArtistsFor(multi, track({ artist: ' 花譜 ' }), 'AA')).toBe(true)
    expect(keepArtistsFor(multi, track({ artist: '' }), '花譜')).toBe(true)
    expect(keepArtistsFor(multi, track({ artist: '' }), 'AA')).toBe(false)
    expect(keepArtistsFor(multi, track({ artist: '花譜; 理芽' }), 'AA')).toBe(false)
  })

  it('draftFrom は提案の keep_artists をファイルから決め、保存値が true でもファイルが多値でなければ false に戻す', () => {
    const it0 = item({
      tracks: [multi, single],
      proposal: {
        ...proposal,
        tracks: [
          { rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: 'one', artist: '花譜;  理芽 ; ', keep_artists: true },
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

  it('保存時に「そのまま保つ」だった artist は現在のファイルの値から作り直す（古い表示文字列を書き戻さない）', () => {
    const savedKeep: InboxDraft = {
      ...proposal,
      tracks: [{ rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: 'one', artist: 'A; B', keep_artists: true }],
    }
    const prop = { ...proposal, tracks: [{ rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: 'one', artist: '' }] }
    // 多値 → 1 値: keep は false に戻り、artist は現在の値（"A; B" を C に上書きしない）
    const toSingle = draftFrom(item({ tracks: [file('d/01.flac', 'flac', [['ARTIST', 'C']])], proposal: prop, draft: savedKeep }))
    expect(toSingle.tracks[0]).toMatchObject({ keep_artists: false, artist: 'C' })
    // 多値 → 別の多値: keep のまま、表示は現在の結合値（外しても "A; B" は復活しない）
    const toOther = draftFrom(
      item({ tracks: [file('d/01.flac', 'flac', [['ARTIST', 'C'], ['ARTIST', 'D']])], proposal: prop, draft: savedKeep }),
    )
    expect(toOther.tracks[0]).toMatchObject({ keep_artists: true, artist: 'C; D' })
    // 保存が false（編集済み）なら保存した値のまま
    const savedEdit: InboxDraft = { ...savedKeep, tracks: [{ ...savedKeep.tracks[0], artist: 'Z', keep_artists: false }] }
    const edited = draftFrom(
      item({ tracks: [file('d/01.flac', 'flac', [['ARTIST', 'C'], ['ARTIST', 'D']])], proposal: prop, draft: savedEdit }),
    )
    expect(edited.tracks[0]).toMatchObject({ keep_artists: false, artist: 'Z' })
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

  it('artworkUrl は承認前は Inbox のファイル、配置済みは Library の画像置き場', () => {
    expect(artworkUrl({ id: 7, state: 'pending' }, h1)).toBe(`/api/inbox/7/artwork/${h1}`)
    expect(artworkUrl({ id: 7, state: 'failed' }, h1, 256)).toBe(`/api/inbox/7/artwork/${h1}`)
    // 配置済みはファイルが Library に移っていて Inbox からは 404
    expect(artworkUrl({ id: 7, state: 'placed' }, h1)).toBe(`/api/artwork/${h1}`)
    expect(artworkUrl({ id: 7, state: 'placed' }, h1, 256)).toBe(`/api/artwork/${h1}?size=256`)
  })
})

describe('watchLabel', () => {
  const t = (e: number) => `T${e}`
  it('監視が無い / 間隔 0 は空、未確認は「まだ」、確認済みは時刻と間隔', () => {
    expect(watchLabel(null, t)).toBe('')
    expect(watchLabel({ checked_at: 5, poll_interval_secs: 0 }, t)).toBe('')
    expect(watchLabel({ checked_at: null, poll_interval_secs: 60 }, t)).toBe('確認: 60 秒ごと（まだ）')
    expect(watchLabel({ checked_at: 5, poll_interval_secs: 60 }, t)).toBe('最後に確認: T5（60 秒ごと）')
  })
})

describe('sameTitleLabel / sameTitleCount', () => {
  const f = (same: InboxFile['same_title']): InboxFile =>
    ({ rel_path: 'a.opus', inode: 1, size: 1, mtime_ns: 0, ctime_ns: 0, codec: 'opus', lossless: false,
       sample_rate: null, bit_depth: null, channels: null, duration_ms: null, tags: [], source: null,
       same_title: same }) as InboxFile

  it('同名が無ければ null、あればパスと長さを並べる（P4-19）', () => {
    expect(sameTitleLabel(f([]))).toBeNull()
    expect(sameTitleLabel(f(undefined as unknown as InboxFile['same_title']))).toBeNull()
    expect(sameTitleLabel(f([{ track_id: 3, rel_path: 'A/B/13 new.flac', duration_ms: 61000 }]))).toBe(
      'Library に同名: 13 new.flac（1:01）',
    )
    expect(
      sameTitleLabel(
        f([
          { track_id: 3, rel_path: 'A/B/13 new.flac', duration_ms: null },
          { track_id: 4, rel_path: 'A/B/14 new.flac', duration_ms: 1000 },
        ]),
      ),
    ).toBe('Library に同名: 13 new.flac、14 new.flac（0:01）')
  })

  it('この曲の長さも並べる（P4-22。二重取り込みか別テイクかを長さで比べる）', () => {
    const own = { ...f([{ track_id: 8363, rel_path: 'A/B/174 再会 (Cover).opus', duration_ms: 245381 }]), duration_ms: 265561 }
    expect(sameTitleLabel(own)).toBe('Library に同名: 174 再会 (Cover).opus（4:05）／この曲 4:26')
  })

  it('件ごとの同名の数を数える', () => {
    expect(sameTitleCount({ tracks: [f([]), f([{ track_id: 1, rel_path: 'x', duration_ms: null }])] } as InboxItem)).toBe(1)
    expect(sameTitleCount({ tracks: [] } as unknown as InboxItem)).toBe(0)
  })
})

describe('applyTracklist（トラックリスト貼り付け。P2-10、D-65）', () => {
  const t = (rel_path: string, disc_no: number, track_no: number, over: Partial<DraftTrack> = {}): DraftTrack => ({
    rel_path,
    disc_no,
    track_no,
    title: '',
    artist: '',
    keep_artists: false,
    ...over,
  })
  const base = (tracks: DraftTrack[]): InboxDraft => ({ ...proposal, tracks })

  it('トラック番号で行に写す（並び順ではない）。アーティストの無い行は既存を保つ', () => {
    const d = base([t('d/b.flac', 1, 2, { artist: 'keep' }), t('d/a.flac', 1, 1), t('d/c.flac', 1, 3)])
    const r = applyTracklist(d, 1, [
      { no: 1, title: 'one', artist: 'X' },
      { no: 2, title: 'two', artist: null },
    ])
    expect(r.draft.tracks.map((x) => [x.rel_path, x.title, x.artist])).toEqual([
      ['d/b.flac', 'two', 'keep'],
      ['d/a.flac', 'one', 'X'],
      ['d/c.flac', '', ''],
    ])
    expect(r.warnings).toEqual(['貼り付けの行数 2 がディスク 1 の 3 曲と違う', '未設定の行: 3'])
    // 元は変えない
    expect(d.tracks[1]!.title).toBe('')
  })

  it('件に無い番号は捨てて警告。全部そろえば警告なし', () => {
    const d = base([t('d/a.flac', 1, 1), t('d/b.flac', 1, 2)])
    const r = applyTracklist(d, 1, [
      { no: 1, title: 'one', artist: null },
      { no: 2, title: 'two', artist: null },
      { no: 3, title: 'three', artist: null },
    ])
    expect(r.draft.tracks.map((x) => x.title)).toEqual(['one', 'two'])
    expect(r.warnings).toEqual(['貼り付けの行数 3 がディスク 1 の 2 曲と違う', 'ディスク 1 に無い番号: 3'])
    const ok = applyTracklist(d, 1, [
      { no: 1, title: 'one', artist: null },
      { no: 2, title: 'two', artist: null },
    ])
    expect(ok.warnings).toEqual([])
  })

  it('複数枚組は選んだディスクの行にだけ写す', () => {
    const d = base([t('d/1-1.flac', 1, 1), t('d/2-1.flac', 2, 1), t('d/2-2.flac', 2, 2)])
    expect(discNumbers(d)).toEqual([1, 2])
    const r = applyTracklist(d, 2, [
      { no: 1, title: 'a', artist: null },
      { no: 2, title: 'b', artist: null },
    ])
    expect(r.draft.tracks.map((x) => x.title)).toEqual(['', 'a', 'b'])
    expect(r.warnings).toEqual([])
  })

  it('同じ番号の行が複数あれば写さずに警告', () => {
    const d = base([t('d/a.flac', 1, 1), t('d/b.flac', 1, 1), t('d/c.flac', 1, 2)])
    const r = applyTracklist(d, 1, [
      { no: 1, title: 'one', artist: null },
      { no: 2, title: 'two', artist: null },
    ])
    expect(r.draft.tracks.map((x) => x.title)).toEqual(['', '', 'two'])
    expect(r.warnings).toEqual(['貼り付けの行数 2 がディスク 1 の 3 曲と違う', '番号が重複する行には写さない: 1'])
  })

  it('アーティストを貼ると多値の「そのまま保つ」を外す（貼った値で 1 値にする）', () => {
    const d = base([t('d/a.flac', 1, 1, { artist: 'P; Q', keep_artists: true }), t('d/b.flac', 1, 2, { artist: 'P; Q', keep_artists: true })])
    const r = applyTracklist(d, 1, [
      { no: 1, title: 'one', artist: 'Z' },
      { no: 2, title: 'two', artist: null },
    ])
    expect(r.draft.tracks.map((x) => [x.artist, x.keep_artists])).toEqual([
      ['Z', false],
      ['P; Q', true],
    ])
  })
})

describe('MusicBrainz の候補を写す（P4-21）', () => {
  const MBID = 'f1223d63-f359-457d-b935-fc27eb24a6de'
  const cand: ReleaseCandidate = {
    release_id: MBID,
    release_group_id: '0b3a4c5d-1111-2222-3333-444455556666',
    title: 'Five',
    artist: '嵐',
    date: '2011-06-22',
    country: 'JP',
    status: 'Official',
    barcode: null,
    disambiguation: null,
    labels: [],
    exact: false,
    matched_by: ['isrc'],
    media: [
      { position: 1, format: 'CD', track_count: 2 },
      { position: 2, format: 'CD', track_count: 2 },
    ],
    medium_position: 2,
    medium_count: 2,
    medium_title: null,
    format: 'CD',
    tracks: [
      { number: '1', position: 1, title: 'Song A', artist: '嵐', length_ms: 1000, recording_id: 'r1', track_id: 't1', isrcs: [] },
      { number: '2', position: 2, title: 'Song B', artist: 'Guest', length_ms: 1000, recording_id: 'r2', track_id: 't2', isrcs: [] },
    ],
  }
  const t = (rel_path: string, track_no: number, title: string, artist = ''): DraftTrack => ({
    rel_path,
    disc_no: 1,
    track_no,
    title,
    artist,
    keep_artists: false,
  })

  it('ID は常に写し、名前は空欄（と Track NN）だけ埋める。手で入れた値は上書きしない', () => {
    const d: InboxDraft = {
      ...proposal,
      albumartist: '',
      album: '手入力',
      date: null,
      release_id: null,
      release_group_id: null,
      tracks: [t('CD/01.flac', 1, 'Track 01'), t('CD/02.flac', 2, '直した', '')],
    }
    const r = applyCandidate(d, cand)
    expect(r.release_id).toBe(MBID)
    expect(r.release_group_id).toBe('0b3a4c5d-1111-2222-3333-444455556666')
    expect([r.albumartist, r.album, r.date]).toEqual(['嵐', '手入力', '2011-06-22'])
    // トラック番号で候補の曲に対応。アルバムアーティストと同じアーティストは空のまま（プレースホルダで見える）
    expect(r.tracks.map((x) => [x.title, x.artist])).toEqual([
      ['Song A', ''],
      ['直した', 'Guest'],
    ])
    // ディスク番号は候補の medium の位置（2 枚目の盤を 2 枚目として置く）
    expect(r.tracks.map((x) => x.disc_no)).toEqual([2, 2])
    // 元は変えない
    expect(d.release_id).toBeNull()
  })

  it('draftFrom はリリース ID を保存した下書き → 提案の順で取り、送信では空を null にする', () => {
    const it0 = item({ proposal: { ...proposal, release_id: MBID, release_group_id: null } })
    expect(draftFrom(it0).release_id).toBe(MBID)
    const saved = { ...proposal, release_id: 'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee' }
    expect(draftFrom(item({ proposal: { ...proposal, release_id: MBID }, draft: saved })).release_id).toBe(
      'aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee',
    )
    expect(draftForSubmit({ ...proposal, release_id: '  ', release_group_id: undefined }).release_id).toBeNull()
  })

  it('validateDraft はリリース ID の形を見る（サーバと同じ文言）', () => {
    const files = proposal.tracks.map((x) => x.rel_path)
    expect(validateDraft({ ...proposal, release_id: MBID }, files)).toEqual([])
    expect(validateDraft({ ...proposal, release_id: 'x' }, files)).toEqual(['MusicBrainz のリリース ID の形が不正: x'])
    // 大文字の UUID も通す（既存のタグ。サーバと同じ）
    expect(validateDraft({ ...proposal, release_id: MBID.toUpperCase() }, files)).toEqual([])
  })
})

describe('承認画面の表の列', () => {
  const f = (tags: Array<[string, string]>) => ({ tags })
  it('extraTagKeys は固定列のタグを外して ABC 順に集める', () => {
    const keys = extraTagKeys([
      f([
        ['TITLE', 'a'],
        ['GENRE', 'Rock'],
        ['PICTURE', 'image/jpeg:ab'],
        ['ISRC', 'X'],
      ]),
      f([
        ['MUSICBRAINZ_ALBUMID', 'm'],
        ['GENRE', 'Pop'],
        ['TRACKNUMBER', '1'],
      ]),
    ])
    expect(keys).toEqual(['GENRE', 'ISRC', 'MUSICBRAINZ_ALBUMID'])
  })
  it('tagValue は多値を "; " で結合し、無ければ空', () => {
    const file = f([
      ['GENRE', 'Rock'],
      ['GENRE', 'Pop'],
    ])
    expect(tagValue(file, 'GENRE')).toBe('Rock; Pop')
    expect(tagValue(file, 'ISRC')).toBe('')
  })
  it('inboxColumns はサムネイル・判定を該当があるときだけ出し、タグ列を末尾に足す', () => {
    const ids = (o: Parameters<typeof inboxColumns>[0]) => inboxColumns(o).map((c) => c.id)
    expect(ids({ hasPicture: false, hasSource: false, tagKeys: [] })).toEqual([
      'disc',
      'no',
      'title',
      'artist',
      'album',
      'albumartist',
      'date',
      'category',
      'duration',
      'codec',
      'file',
    ])
    const all = ids({ hasPicture: true, hasSource: true, tagKeys: ['GENRE'] })
    expect(all[2]).toBe('thumb')
    expect(all.slice(-2)).toEqual(['verdict', 'tag:GENRE'])
  })
  it('番号とタイトルは隠せない', () => {
    const fixed = inboxColumns({ hasPicture: false, hasSource: false, tagKeys: [] })
      .filter((c) => !c.hideable)
      .map((c) => c.id)
    expect(fixed).toEqual(['disc', 'no', 'title'])
  })
  it('parseHiddenColumns は壊れた値を空にする', () => {
    expect(parseHiddenColumns(null)).toEqual([])
    expect(parseHiddenColumns('{')).toEqual([])
    expect(parseHiddenColumns('{"a":1}')).toEqual([])
    expect(parseHiddenColumns('["tag:GENRE", 3]')).toEqual(['tag:GENRE'])
  })
})

describe('stickyColumns', () => {
  it('先頭から続く固定列の left を累積し、最後の列に印を付ける', () => {
    const m = stickyColumns([{ id: 'disc' }, { id: 'no' }, { id: 'thumb' }, { id: 'title' }, { id: 'artist' }])
    expect(m.get('disc')).toEqual({ left: 0, width: 72, last: false })
    expect(m.get('thumb')).toEqual({ left: 144, width: 44, last: false })
    expect(m.get('title')).toEqual({ left: 188, width: 292, last: true })
    expect(m.has('artist')).toBe(false)
  })
  it('画像の列を隠すとタイトルが詰まる', () => {
    const m = stickyColumns([{ id: 'disc' }, { id: 'no' }, { id: 'title' }])
    expect(m.get('title')).toEqual({ left: 144, width: 292, last: true })
  })
})

describe('タグの変更（D-86）', () => {
  const f = file('d/01.flac', 'flac', [
    ['GENRE', 'Rock'],
    ['GENRE', 'Pop'],
    ['COMMENT', 'c'],
  ])
  const t: DraftTrack = { rel_path: 'd/01.flac', disc_no: 1, track_no: 1, title: 'one', artist: '' }
  it('setTrackTag はファイルの値と違うときだけ変更を持つ', () => {
    const a = setTrackTag(t, f, 'GENRE', 'Jazz; Soul')
    expect(a.tags).toEqual({ GENRE: ['Jazz', 'Soul'] })
    expect(effectiveTag(f, a, 'GENRE')).toBe('Jazz; Soul')
    expect(tagChanged(f, a, 'GENRE')).toBe(true)
    // 元に戻すと変更を消す
    const b = setTrackTag(a, f, 'GENRE', 'Rock;Pop')
    expect(b.tags).toEqual({})
    expect(tagChanged(f, b, 'GENRE')).toBe(false)
  })
  it('空にするとファイルにあるキーは消し、無いキーは変更を持たない', () => {
    expect(setTrackTag(t, f, 'COMMENT', '  ').tags).toEqual({ COMMENT: null })
    expect(effectiveTag(f, setTrackTag(t, f, 'COMMENT', ''), 'COMMENT')).toBe('')
    expect(setTrackTag(t, f, 'LYRICIST', '').tags).toEqual({})
    expect(setTrackTag(t, f, 'LYRICIST', 'L').tags).toEqual({ LYRICIST: ['L'] })
  })
  it('newTagKeyProblem は形・上の欄のキー・鍵・既存の列を弾き、小文字は大文字にして通す', () => {
    expect(newTagKeyProblem('lyricist', ['GENRE'])).toBeNull()
    expect(newTagKeyProblem('', [])).not.toBeNull()
    expect(newTagKeyProblem('A=B', [])).not.toBeNull()
    expect(newTagKeyProblem('title', [])).toContain('TITLE')
    expect(newTagKeyProblem('source_url', [])).toContain('識別')
    expect(newTagKeyProblem('genre', ['GENRE'])).toContain('既にある')
    expect(isLockedTagKey('MUSICBRAINZ_DISCID')).toBe(true)
    expect(isLockedTagKey('GENRE')).toBe(false)
  })
  it('validateDraft はサーバと同じ規則でタグのキーと画像を検証する', () => {
    const d = draftFrom(item())
    const files = ['d/01.flac', 'd/02.flac']
    expect(validateDraft({ ...d, tracks: [{ ...d.tracks[0], tags: { GENRE: ['x'] } }, d.tracks[1]] }, files)).toEqual([])
    const bad = (tags: Record<string, string[] | null>) =>
      validateDraft({ ...d, tracks: [{ ...d.tracks[0], tags }, d.tracks[1]] }, files)
    expect(bad({ genre: ['x'] })[0]).toContain('タグのキーが不正')
    expect(bad({ TITLE: ['x'] })[0]).toContain('上の欄で直す')
    expect(bad({ SOURCE_URL: ['x'] })[0]).toContain('識別')
    const pic = (picture: string) =>
      validateDraft({ ...d, tracks: [{ ...d.tracks[0], picture }, d.tracks[1]] }, files)
    expect(pic(`image/png:${'a'.repeat(64)}`)).toEqual([])
    expect(pic(`image/gif:${'a'.repeat(64)}`)[0]).toContain('画像の指定が不正')
  })
  it('draftForSubmit はタグの空の値を null にし、変更の無い欄は送らない', () => {
    const d = draftFrom(item())
    const out = draftForSubmit({
      ...d,
      tracks: [{ ...d.tracks[0], tags: { GENRE: [' Jazz ', ''], COMMENT: [] }, picture: `image/png:${'b'.repeat(64)}` }, d.tracks[1]],
    })
    expect(out.tracks[0].tags).toEqual({ GENRE: ['Jazz'], COMMENT: null })
    expect(out.tracks[0].picture).toBe(`image/png:${'b'.repeat(64)}`)
    expect('tags' in out.tracks[1]).toBe(false)
    expect('picture' in out.tracks[1]).toBe(false)
  })
  it('保存済みの下書きのタグの変更と画像は draftFrom で戻る', () => {
    const saved: InboxDraft = {
      ...proposal,
      tracks: [{ ...proposal.tracks[0], tags: { GENRE: ['Jazz'] }, picture: `image/png:${'c'.repeat(64)}` }, proposal.tracks[1]],
    }
    const d = draftFrom(item({ draft: saved }))
    expect(d.tracks[0].tags).toEqual({ GENRE: ['Jazz'] })
    expect(d.tracks[0].picture).toBe(`image/png:${'c'.repeat(64)}`)
    expect(d.tracks[1].tags).toBeUndefined()
  })
})

describe('画像の差し替え（D-86）', () => {
  const h = (c: string) => c.repeat(64)
  const pic = (c: string) => ['PICTURE', `image/jpeg:${h(c)}`] as [string, string]
  const withFiles = (tags: Array<Array<[string, string]>>) => {
    const fs = tags.map((t, i) => file(`d/0${i + 1}.flac`, 'flac', t))
    const d: InboxDraft = {
      ...proposal,
      tracks: fs.map((f, i) => ({ rel_path: f.rel_path, disc_no: 1, track_no: i + 1, title: 't', artist: '' })),
    }
    return { files: new Map(fs.map((f) => [f.rel_path, f])), d }
  }
  it('usesCaaPicture は提案に入れた Cover Art Archive の画像が全曲にそのまま入っているときだけ true（D-91）', () => {
    const caa = `image/png:${h('c')}`
    const { d } = withFiles([[], []])
    const all = { ...d, tracks: d.tracks.map((t) => ({ ...t, picture: caa })) }
    expect(usesCaaPicture({ caa_picture: caa }, all)).toBe(true)
    // 外した（resetPictures）・1 曲だけ差し替えた・取っていない（旧サーバ含む）は false
    expect(usesCaaPicture({ caa_picture: caa }, resetPictures(all))).toBe(false)
    const one = { ...all, tracks: all.tracks.map((t, i) => (i === 0 ? { ...t, picture: `image/jpeg:${h('d')}` } : t)) }
    expect(usesCaaPicture({ caa_picture: caa }, one)).toBe(false)
    expect(usesCaaPicture({ caa_picture: null }, all)).toBe(false)
    expect(usesCaaPicture({}, all)).toBe(false)
    expect(usesCaaPicture({ caa_picture: caa }, { tracks: [] })).toBe(false)
  })
  it('refreshedDraft は人が触っていないときだけ新しい初期値を返す（走査で表の画像が入った。D-91）', () => {
    const { d } = withFiles([[], []])
    const withPic = { ...d, tracks: d.tracks.map((t) => ({ ...t, picture: `image/png:${h('c')}` })) }
    // 未編集: 提案に画像が入ったら取り込む
    expect(refreshedDraft(d, d, withPic)).toEqual(withPic)
    // 編集中: 上書きしない
    const edited = { ...d, album: '直した' }
    expect(refreshedDraft(d, edited, withPic)).toBeNull()
    // 初期値が変わっていない（一覧を取り直しただけ）: 何もしない
    expect(refreshedDraft(d, d, structuredClone(d))).toBeNull()
  })
  it('itemThumbUrl は埋め込み画像、無ければ下書き・提案の画像を出す（D-91）', () => {
    const caa = `image/png:${h('c')}`
    const { files, d } = withFiles([[], []])
    const base = { id: 7, state: 'pending' as const, tracks: [...files.values()], draft: null, proposal: d }
    expect(itemThumbUrl(base)).toBeNull()
    const proposal = { ...d, tracks: d.tracks.map((t) => ({ ...t, picture: caa })) }
    expect(itemThumbUrl({ ...base, proposal })).toBe(`/api/artwork/${h('c')}?size=256`)
    // 保存した下書きが画像を外していれば出さない
    expect(itemThumbUrl({ ...base, proposal, draft: d })).toBeNull()
    // ファイルの埋め込み画像が先
    const emb = withFiles([[pic('a')], [pic('a')]])
    expect(itemThumbUrl({ id: 7, state: 'pending', tracks: [...emb.files.values()], draft: null, proposal })).toBe(`/api/inbox/7/artwork/${h('a')}`)
    // 配置済みは Library の画像置き場から（Inbox のファイルはもう無い）
    expect(itemThumbUrl({ id: 7, state: 'placed', tracks: [...emb.files.values()], draft: null, proposal })).toBe(`/api/artwork/${h('a')}?size=256`)
  })
  it('pictureState は無し / 全曲同じ / 曲ごとに違う（一部無し）を見分ける', () => {
    expect(pictureState(withFiles([[], []]).files, withFiles([[], []]).d)).toMatchObject({ mode: 'none', missing: 2 })
    const u = withFiles([[pic('a')], [pic('a')]])
    expect(pictureState(u.files, u.d)).toMatchObject({ mode: 'uniform', kinds: 1, missing: 0 })
    const m = withFiles([[pic('a')], [pic('b')], []])
    expect(pictureState(m.files, m.d)).toEqual({ mode: 'mixed', kinds: 2, missing: 1, changed: 0 })
  })
  it('applyPicture は全曲 / 画像の無い曲 / 1 曲に当て、resetPictures で戻る', () => {
    const { files, d } = withFiles([[pic('a')], [pic('b')], []])
    const v = `image/png:${h('e')}`
    const missing = applyPicture(d, files, v, 'missing')
    expect(missing.tracks.map((t) => t.picture ?? null)).toEqual([undefined, undefined, v].map((x) => x ?? null))
    expect(pictureState(files, missing)).toMatchObject({ mode: 'mixed', kinds: 3, missing: 0, changed: 1 })
    const one = applyPicture(d, files, v, 1)
    expect(one.tracks[1].picture).toBe(v)
    expect(one.tracks[0].picture).toBeUndefined()
    const all = applyPicture(d, files, v, 'all')
    expect(pictureState(files, all)).toMatchObject({ mode: 'uniform', changed: 3 })
    expect(pictureState(files, resetPictures(all))).toMatchObject({ mode: 'mixed', changed: 0 })
  })
  it('trackPictureUrl は差し替えた画像を /api/artwork、ファイルの画像を件の埋め込み画像で引く', () => {
    const f = file('d/01.flac', 'flac', [pic('a')])
    const pending = { id: 7, state: 'pending' as const }
    expect(trackPictureUrl(pending, f, {})).toBe(`/api/inbox/7/artwork/${h('a')}`)
    expect(trackPictureUrl(pending, f, { picture: `image/png:${h('f')}` })).toBe(`/api/artwork/${h('f')}`)
    expect(trackPictureUrl(pending, file('d/02.flac'), {})).toBeNull()
    // 配置済みのファイルの画像は Library の画像置き場
    expect(trackPictureUrl({ id: 7, state: 'placed' }, f, {})).toBe(`/api/artwork/${h('a')}`)
  })
})

describe('列の編集可否（D-86）', () => {
  it('アルバム単位の列とタグは直せ、鍵のタグ・DISCTOTAL・ファイル由来は直せない', () => {
    const cols = inboxColumns({ hasPicture: true, hasSource: false, tagKeys: ['DISCTOTAL', 'GENRE', 'SOURCE_URL'] })
    const ed = Object.fromEntries(cols.map((c) => [c.id, c.editable]))
    expect(ed).toMatchObject({
      disc: true,
      thumb: true,
      album: true,
      date: true,
      category: false,
      duration: false,
      file: false,
      'tag:GENRE': true,
      'tag:DISCTOTAL': false,
      'tag:SOURCE_URL': false,
    })
    expect(cols.find((c) => c.id === 'tag:SOURCE_URL')?.locked).toBe(true)
  })
  it('extraTagKeys は下書きで足したキーも列にする（消すだけのキーは足さない）', () => {
    const keys = extraTagKeys([file('d/01.flac', 'flac', [['GENRE', 'x']])], [{ tags: { LYRICIST: ['l'], MOOD: null } }])
    expect(keys).toEqual(['GENRE', 'LYRICIST'])
  })
})

describe('draftChangeCount（D-86）', () => {
  it('提案からの変更を欄・トラック・タグ・画像ごとに数える', () => {
    const d = draftFrom(item())
    expect(draftChangeCount(item(), d)).toBe(0)
    const e: InboxDraft = {
      ...d,
      album: 'Y',
      tracks: [
        { ...d.tracks[0], title: 'uno', tags: { GENRE: ['x'], COMMENT: null }, picture: `image/png:${'a'.repeat(64)}` },
        d.tracks[1],
      ],
    }
    expect(draftChangeCount(item(), e)).toBe(5)
    // album gain の基準は追記先の現在値
    const dest = item({ destination: { album_id: 1, album: 'X', track_count: 2, max_track_no: 2, album_gain: true } })
    expect(draftChangeCount(dest, draftFrom(dest))).toBe(0)
  })
})
