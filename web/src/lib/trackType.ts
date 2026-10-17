// 曲の種類（TRACK_TYPE）と 00_ から外す印（PLAYLIST_EXCLUDE）の値と、Inbox の承認画面で出す候補（D-113）。
// 種類は 4 つで、タグが無ければ普通の曲。候補はタイトルとアルバム名から出し、人が選んで適用する

export const TRACK_TYPE_KEY = 'TRACK_TYPE'
export const PLAYLIST_EXCLUDE_KEY = 'PLAYLIST_EXCLUDE'

export type TrackType = 'karaoke' | 'bgm' | 'drama' | 'talk'

export const TRACK_TYPES: ReadonlyArray<{ value: TrackType; label: string }> = [
  { value: 'karaoke', label: 'カラオケ' },
  { value: 'bgm', label: 'BGM' },
  { value: 'drama', label: 'ドラマ' },
  { value: 'talk', label: 'トーク' },
]

/** 表示名。空は普通の曲、知らない値（手で書いたもの）はそのまま */
export function trackTypeLabel(value: string): string {
  if (value === '') return '曲'
  return TRACK_TYPES.find((t) => t.value === value)?.label ?? value
}

const KARAOKE = /instrumental|\binst\.|off[\s-]?vocal|karaoke|カラオケ|歌無し|without vocal|backing track/i
const DRAMA = /ドラマ|\bdrama\b|ボーナス・?トラック/i
const TALK = /^mc\s*\d*$|ラジオ|トーク|コメント|DJトラック/i
const SOUNDTRACK = /サウンドトラック|サントラ|soundtrack|sound track|\bost\b|music collection|劇伴/i

/** アルバムで一番多いアーティスト（作曲者の推定）。同数なら先に出たもの */
export function mainArtist(artists: readonly string[]): string | null {
  // Map は最初に出た順を保つので、同数なら先に出たものが残る
  const count = new Map<string, number>()
  for (const a of artists) count.set(a, (count.get(a) ?? 0) + 1)
  let best: string | null = null
  for (const [a, n] of count) if (best == null || n > (count.get(best) ?? 0)) best = a
  return best
}

/**
 * 種類の候補。タイトルから karaoke / drama / talk を、サントラ（アルバム名）で作曲者（アルバムで一番多い
 * アーティスト）の曲を bgm とみなす。当てはまらなければ null（普通の曲）
 */
export function suggestTrackType(
  track: { title: string; artist: string },
  album: { album: string; mainArtist: string | null },
): TrackType | null {
  const title = track.title.trim()
  if (KARAOKE.test(title)) return 'karaoke'
  if (DRAMA.test(title)) return 'drama'
  if (TALK.test(title)) return 'talk'
  if (SOUNDTRACK.test(album.album) && album.mainArtist != null && track.artist === album.mainArtist) return 'bgm'
  return null
}
