import { describe, expect, it } from 'vitest'
import { libraryNotice, libraryOnInput, libraryOnLoaded, type AlbumRef, type LibraryState } from './cdLibrary'

const alb: AlbumRef = { album_id: 7, rel_dir: 'Rock/Nirvana/Nevermind', album: 'Nevermind', albumartist: 'Nirvana' }

describe('libraryNotice', () => {
  it('当たりが無ければ帯を出さない', () => {
    expect(libraryNotice(null)).toBeNull()
    expect(libraryNotice({ discid: 'x', disc: null, release: null })).toBeNull()
  })
  it('盤そのものが当たれば disc', () => {
    expect(libraryNotice({ discid: 'x', disc: alb, release: null })).toEqual({
      kind: 'disc',
      text: 'この盤はライブラリにある: 『Nevermind』（Nirvana）',
      albumId: 7,
    })
  })
  it('リリースだけ当たれば release', () => {
    const n = libraryNotice({ discid: 'x', disc: null, release: alb })
    expect(n?.kind).toBe('release')
    expect(n?.albumId).toBe(7)
  })
  it('アルバム名が無ければディレクトリで示す', () => {
    const n = libraryNotice({ discid: 'x', disc: { ...alb, album: null, albumartist: null }, release: null })
    expect(n?.text).toBe('この盤はライブラリにある: 『Rock/Nirvana/Nevermind』')
  })
})

describe('useCdLibrary の状態', () => {
  const res = (album_id: number) => ({ discid: 'x', disc: { ...alb, album_id }, release: null })
  it('A → B → A と戻ると、2 回目の A の応答までは何も出さない', () => {
    let s: LibraryState = { key: '', value: null }
    s = libraryOnInput(s, 'A')
    s = libraryOnLoaded(s, 'A', res(1))
    expect(s.value?.disc?.album_id).toBe(1)
    s = libraryOnInput(s, 'B')
    expect(s.value).toBeNull()
    s = libraryOnInput(s, 'A')
    expect(s.value).toBeNull()
    s = libraryOnLoaded(s, 'A', res(2))
    expect(s.value?.disc?.album_id).toBe(2)
  })
  it('前の入力への応答は捨てる', () => {
    let s = libraryOnInput({ key: '', value: null }, 'A')
    s = libraryOnInput(s, 'B')
    s = libraryOnLoaded(s, 'A', res(1))
    expect(s).toEqual({ key: 'B', value: null })
  })
  it('同じ入力なら結果を保つ（再描画で消さない）', () => {
    const s = libraryOnLoaded(libraryOnInput({ key: '', value: null }, 'A'), 'A', res(1))
    expect(libraryOnInput(s, 'A')).toBe(s)
  })
  it('失敗は帯を出さない', () => {
    let s = libraryOnLoaded(libraryOnInput({ key: '', value: null }, 'A'), 'A', res(1))
    s = libraryOnInput(s, 'B')
    s = libraryOnLoaded(s, 'B', null)
    expect(s.value).toBeNull()
  })
})
