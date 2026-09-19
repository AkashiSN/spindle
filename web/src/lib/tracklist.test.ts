import { describe, expect, it } from 'vitest'
import { parseTracklist } from './tracklist'

const titles = (text: string, artistFirst = false) => parseTracklist(text, { artistFirst }).tracks.map((t) => t.title)

describe('parseTracklist: 番号', () => {
  it('先頭の番号を拾う: 1. / 01 / 1) / [01] / Track 1: / M-1 / #1 / 全角', () => {
    const r = parseTracklist(
      ['1. いち', '02 に', '3) さん', '[04] よん', 'Track 5: ご', 'M-6 ろく', '#7 なな', '８．はち', 'M9 きゅう', '10、じゅう'].join(
        '\n',
      ),
    )
    expect(r.tracks.map((t) => [t.no, t.title])).toEqual([
      [1, 'いち'],
      [2, 'に'],
      [3, 'さん'],
      [4, 'よん'],
      [5, 'ご'],
      [6, 'ろく'],
      [7, 'なな'],
      [8, 'はち'],
      [9, 'きゅう'],
      [10, 'じゅう'],
    ])
    expect(r.warnings).toEqual([])
  })
  it('番号の無い行は順に振る。一部だけ番号があれば前の行 + 1', () => {
    expect(parseTracklist('a\nb\nc').tracks.map((t) => t.no)).toEqual([1, 2, 3])
    expect(parseTracklist('3. a\nb\n7. c\nd').tracks.map((t) => t.no)).toEqual([3, 4, 7, 8])
  })
  it('年や 4 桁の数字で始まるタイトルは番号にしない。100 以上も番号にしない', () => {
    expect(titles('2024 Overture\n100 Years')).toEqual(['2024 Overture', '100 Years'])
    expect(parseTracklist('2024 Overture').tracks[0]!.no).toBe(1)
  })
  it('番号だけの行はタイトル扱い。空白だけの区切りも番号（"7 Rings" は 7 番の "Rings"。プレビューで直す）', () => {
    expect(titles('1\n12')).toEqual(['1', '12'])
    expect(parseTracklist('7 Rings').tracks[0]).toMatchObject({ no: 7, title: 'Rings' })
  })
  it('番号の重複と飛びは警告', () => {
    const r = parseTracklist('1. a\n1. b\n5. c')
    expect(r.tracks.map((t) => t.no)).toEqual([1, 1, 5])
    expect(r.warnings.join('\n')).toMatch(/重複/)
    expect(r.warnings.join('\n')).toMatch(/連番/)
  })
})

describe('parseTracklist: 空行・見出し・時間', () => {
  it('空行と Disc / トラックリストの見出しは飛ばす', () => {
    const r = parseTracklist('トラックリスト\n\nDisc 1\n1. a\n\n  \nDISC 2:\n2. b\n収録曲：\n')
    expect(r.tracks.map((t) => t.title)).toEqual(['a', 'b'])
    expect(r.warnings.join('\n')).toMatch(/見出し/)
  })
  it('末尾の時間を長さとして拾い、タイトルから外す', () => {
    const r = parseTracklist('1. a 4:32\n2. b (3:05)\n3. c [12:00]\n4. d\t1:02:03\n5. e （0:59）')
    expect(r.tracks.map((t) => [t.title, t.length_ms])).toEqual([
      ['a', 272000],
      ['b', 185000],
      ['c', 720000],
      ['d', 3723000],
      ['e', 59000],
    ])
  })
  it('時間は全角の数字・コロンも受ける（行末・括弧付き・タブの列）。タイトルの全角は触らない', () => {
    const r = parseTracklist('１．曲名 （４：３２）\n2. 第２章 ３：０５\n3. 曲\t１２：００\n4. 曲２（12:00）')
    expect(r.tracks.map((t) => [t.no, t.title, t.length_ms])).toEqual([
      [1, '曲名', 272000],
      [2, '第２章', 185000],
      [3, '曲', 720000],
      [4, '曲２', 720000],
    ])
  })
  it('時間に見えない数字はタイトルに残す', () => {
    expect(titles('1. Room 101\n2. 24:7 Love\n3. 9:99')).toEqual(['Room 101', '24:7 Love', '9:99'])
  })
})

describe('parseTracklist: アーティストの区切り', () => {
  it('タイトル / アーティスト（既定）。全角スラッシュと空白無しも', () => {
    const r = parseTracklist('1. Title / Artist\n2. タイトル／歌手\n3. only')
    expect(r.tracks.map((t) => [t.title, t.artist])).toEqual([
      ['Title', 'Artist'],
      ['タイトル', '歌手'],
      ['only', null],
    ])
  })
  it('ダッシュ・縦棒も区切り。区切りはアーティスト側の端で切る（タイトル内の " - " は残る）', () => {
    const r = parseTracklist('1. Title - Remix - Artist\n2. T ｜ A\n3. T – A\n4. T — A')
    expect(r.tracks.map((t) => [t.title, t.artist])).toEqual([
      ['Title - Remix', 'Artist'],
      ['T', 'A'],
      ['T', 'A'],
      ['T', 'A'],
    ])
  })
  it('artistFirst で左右が入れ替わる。アーティスト側の端で切る', () => {
    const r = parseTracklist('1. Artist - Title - Remix\n2. 歌手／タイトル', { artistFirst: true })
    expect(r.tracks.map((t) => [t.title, t.artist])).toEqual([
      ['Title - Remix', 'Artist'],
      ['タイトル', '歌手'],
    ])
  })
  it('スラッシュが縦棒・ダッシュより優先', () => {
    const r = parseTracklist('1. A - B / C')
    expect(r.tracks[0]).toMatchObject({ title: 'A - B', artist: 'C' })
  })
  it('feat. は区切らない。端にぶら下がった区切りは落とし、空白無しの - は残す', () => {
    const r = parseTracklist('1. Title feat. X\n2. Title /\n3. / Artist\n4. Title／\n5. -Intro-')
    expect(r.tracks.map((t) => [t.title, t.artist])).toEqual([
      ['Title feat. X', null],
      ['Title', null],
      ['Artist', null],
      ['Title', null],
      ['-Intro-', null],
    ])
  })
  it('端の区切りを落としてから、アーティスト側の端で切る', () => {
    expect(parseTracklist('1. Title／Artist／').tracks[0]).toMatchObject({ title: 'Title', artist: 'Artist' })
    expect(parseTracklist('1. ／Artist／Title', { artistFirst: true }).tracks[0]).toMatchObject({
      title: 'Title',
      artist: 'Artist',
    })
    expect(parseTracklist('1. A - B - ').tracks[0]).toMatchObject({ title: 'A', artist: 'B' })
    expect(parseTracklist('1. A - B -／').tracks[0]).toMatchObject({ title: 'A', artist: 'B' })
  })
})

describe('parseTracklist: タブ区切り（表の貼り付け）', () => {
  it('列を 番号・タイトル・アーティスト・時間 に振る。数字だけの列は番号、m:ss は時間', () => {
    const r = parseTracklist('1\tSmells Like Teen Spirit\tNirvana\t5:01\n2\tIn Bloom\tNirvana\t4:14')
    expect(r.tracks).toEqual([
      { no: 1, title: 'Smells Like Teen Spirit', artist: 'Nirvana', length_ms: 301000 },
      { no: 2, title: 'In Bloom', artist: 'Nirvana', length_ms: 254000 },
    ])
  })
  it('タブ区切りでも artistFirst と 2 列（タイトルだけ）が効く', () => {
    const r = parseTracklist('Nirvana\tIn Bloom\n\t\tLithium\t', { artistFirst: true })
    expect(r.tracks.map((t) => [t.title, t.artist])).toEqual([
      ['In Bloom', 'Nirvana'],
      ['Lithium', null],
    ])
  })
  it('タブ区切りの列内では " / " を分割しない', () => {
    const r = parseTracklist('1\tA / B\tC')
    expect(r.tracks[0]).toMatchObject({ title: 'A / B', artist: 'C' })
  })
})

describe('parseTracklist: 全体', () => {
  it('何も無ければ空', () => {
    expect(parseTracklist('')).toEqual({ tracks: [], warnings: [] })
    expect(parseTracklist('\n \n')).toEqual({ tracks: [], warnings: [] })
  })
  it('CRLF と前後の空白', () => {
    expect(titles('  1. a  \r\n2. b\r\n')).toEqual(['a', 'b'])
  })
})
