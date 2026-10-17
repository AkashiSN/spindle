import { describe, expect, it } from 'vitest'
import { mainArtist, suggestTrackType, trackTypeLabel } from './trackType'

const album = (name: string, main: string | null = 'Composer') => ({ album: name, mainArtist: main })

describe('suggestTrackType', () => {
  it('カラオケ・instrumental・off vocal・歌無しは karaoke', () => {
    for (const title of ['Five (オリジナル・カラオケ)', '劇薬中毒 (instrumental)', 'Snow halation (Off Vocal)', '銀河航路 (歌無し)', 'Song -inst.-', 'X (Karaoke)']) {
      expect(suggestTrackType({ title, artist: 'A' }, album('Single')), title).toBe('karaoke')
    }
  })
  it('ドラマ・ボーナストラックは drama', () => {
    for (const title of ['オリジナルドラマ 「パジャマパーティー」', 'ドラマ「Dear Trainer」', 'ボーナス・トラック', 'Drama Part 1']) {
      expect(suggestTrackType({ title, artist: 'A' }, album('Single')), title).toBe('drama')
    }
  })
  it('MC・ラジオ・トーク・コメント・DJ トラックは talk', () => {
    for (const title of ['MC1', 'MC', 'mc 12', 'Charlotteラジオ 第1回', 'スペシャルトーク', 'メンバーコメント', 'テーマソングDJCD [DJトラック]']) {
      expect(suggestTrackType({ title, artist: 'A' }, album('Single')), title).toBe('talk')
    }
  })
  it('サントラのアルバムで作曲者（アルバムで一番多いアーティスト）の曲は bgm、歌手の曲は候補なし', () => {
    const ost = album('宇宙戦艦ヤマト2199 オリジナル・サウンドトラック Part.1', '宮川彬良')
    expect(suggestTrackType({ title: '大志', artist: '宮川彬良' }, ost)).toBe('bgm')
    expect(suggestTrackType({ title: '宇宙戦艦ヤマト', artist: 'ささきいさお' }, ost)).toBeNull()
    for (const name of ['X Original Soundtrack', 'Y ORIGINAL SOUND TRACK', 'Z OST', 'まどか Music Collection', 'W サントラ']) {
      expect(suggestTrackType({ title: 't', artist: 'Composer' }, album(name)), name).toBe('bgm')
    }
  })
  it('普通の曲は候補なし。サントラでも作曲者が分からなければ候補なし', () => {
    expect(suggestTrackType({ title: 'ライラック', artist: 'Mrs. GREEN APPLE' }, album('10'))).toBeNull()
    expect(suggestTrackType({ title: 't', artist: 'X' }, album('X Original Soundtrack', null))).toBeNull()
    // 英単語の一部に MC / OST / instrumental / soundtrack を含むだけのものは拾わない
    expect(suggestTrackType({ title: 'MCMXCIX', artist: 'A' }, album('Hostile'))).toBeNull()
    expect(suggestTrackType({ title: 'Instrumentality', artist: 'A' }, album('Single'))).toBeNull()
    expect(suggestTrackType({ title: 't', artist: 'Composer' }, album('The Soundtrackers'))).toBeNull()
  })
})

describe('mainArtist', () => {
  it('一番多いアーティスト。同数なら先に出たもの。空なら null', () => {
    expect(mainArtist(['a', 'b', 'b', 'a', 'c'])).toBe('a')
    expect(mainArtist(['x', 'y', 'y'])).toBe('y')
    expect(mainArtist([])).toBeNull()
    // 空（アーティストが分からない行）は数えない。全部空なら null（サントラの全行を BGM にしない）
    expect(mainArtist(['', '', 'a'])).toBe('a')
    expect(mainArtist(['', ' '])).toBeNull()
  })
})

describe('trackTypeLabel', () => {
  it('既知の値は日本語、知らない値はそのまま、空は「曲」', () => {
    expect(trackTypeLabel('karaoke')).toBe('カラオケ')
    expect(trackTypeLabel('bgm')).toBe('BGM')
    expect(trackTypeLabel('custom')).toBe('custom')
    expect(trackTypeLabel('')).toBe('曲')
  })
})
