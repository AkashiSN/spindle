export function formatDuration(ms: number | null): string {
  if (ms == null) return ''
  const total = Math.round(ms / 1000)
  const h = Math.floor(total / 3600)
  const m = Math.floor((total % 3600) / 60)
  const s = total % 60
  const mm = h > 0 ? String(m).padStart(2, '0') : String(m)
  return h > 0 ? `${h}:${mm}:${String(s).padStart(2, '0')}` : `${mm}:${String(s).padStart(2, '0')}`
}

export function formatCount(n: number | null | undefined): string {
  if (n == null) return '…'
  return n.toLocaleString('ja-JP')
}

/** トラック番号列。ディスク番号は 2 枚目以降だけ前置する（"2-03"）。1 枚物は "03" */
export function formatTrackNo(disc: number | null, track: number | null): string {
  if (track == null) return ''
  const t = String(track).padStart(2, '0')
  return disc != null && disc > 1 ? `${disc}-${t}` : t
}

/** foobar2000 の Artist/album 列（`%album artist% - %album%`）。片方だけならそれだけ */
export function formatArtistAlbum(r: { albumartist: string | null; album: string | null }): string {
  return [r.albumartist, r.album].filter((v): v is string => !!v).join(' - ')
}

/** foobar2000 の Title / track artist 列（`%title%[ // %track artist%]`。アルバムアーティストと違うときだけ） */
export function formatTitleArtist(r: {
  title: string | null
  artist_display: string | null
  albumartist: string | null
}): string {
  const title = r.title ?? ''
  const artist = r.artist_display
  return artist && artist !== r.albumartist ? `${title} // ${artist}` : title
}
