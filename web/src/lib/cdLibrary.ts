// CD 画面の「ライブラリにある」（§12.6 CD、`POST /api/cd/library`）の純粋ロジック

export type AlbumRef = {
  album_id: number
  rel_dir: string
  album: string | null
  albumartist: string | null
}

export type LibraryResponse = {
  discid: string
  /** その盤そのもの（トラックの MUSICBRAINZ_DISCID が一致）を持つ album */
  disc: AlbumRef | null
  /** disc が無いとき、選んだ候補と同じリリースの album */
  release: AlbumRef | null
}

export type LibraryNotice = { kind: 'disc' | 'release'; text: string; albumId: number }

function albumName(a: AlbumRef): string {
  const album = a.album?.trim() || a.rel_dir
  const artist = a.albumartist?.trim()
  return artist ? `『${album}』（${artist}）` : `『${album}』`
}

/** 画面上部の帯の文言。どちらにも当たらなければ null（帯を出さない） */
export function libraryNotice(r: LibraryResponse | null): LibraryNotice | null {
  if (r == null) return null
  if (r.disc != null) {
    return { kind: 'disc', text: `この盤はライブラリにある: ${albumName(r.disc)}`, albumId: r.disc.album_id }
  }
  if (r.release != null) {
    return {
      kind: 'release',
      text: `同じリリースの album がライブラリにある（この盤は未取り込み）: ${albumName(r.release)}`,
      albumId: r.release.album_id,
    }
  }
  return null
}

/**
 * `useCdLibrary` の状態。`key` は照会の入力（TOC とリリース）で、`value` はその入力に対して**今回の訪問で**
 * 返った応答。入力が変わるたびに捨てるので、A → B → A と戻っても 1 回目の A の結果は出ない
 * （所持状態が変わっていれば古い帯が一時的に復活してしまう）
 */
export type LibraryState = { key: string; value: LibraryResponse | null }

/** 入力が変わった: 違う鍵なら結果を捨てる */
export function libraryOnInput(s: LibraryState, key: string): LibraryState {
  return s.key === key ? s : { key, value: null }
}

/** 応答が返った: いまの鍵に対するものだけ採る（失敗は value = null） */
export function libraryOnLoaded(s: LibraryState, key: string, value: LibraryResponse | null): LibraryState {
  return s.key === key ? { key, value } : s
}
