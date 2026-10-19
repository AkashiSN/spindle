import { describe, expect, it } from 'vitest'
import { formatRoute, parseRoute, type Route } from './route'

describe('route', () => {
  it('タブとパラメータを往復できる', () => {
    const rs: Route[] = [
      { tab: 'library', view: 'albums' },
      { tab: 'library', view: 'artists', albumartist: 'ヰ世界情緒' },
      { tab: 'library', view: 'albums', albumId: 12 },
      { tab: 'library', view: 'tracks', q: '夜に' },
      { tab: 'folders', path: ['J-Pop', 'YOASOBI'] },
      { tab: 'playlists', playlistId: 3, edit: 'rule' },
      { tab: 'subscriptions', subscriptionId: 5 },
    ]
    for (const r of rs) {
      const u = new URL(formatRoute(r), 'http://x')
      expect(parseRoute(u.pathname, u.search)).toEqual(r)
    }
  })

  it('知らないパスはライブラリ（前回の見方は呼び出し側が補う）', () => {
    expect(parseRoute('/m/', '')).toEqual({ tab: 'library', view: 'albums' })
    expect(parseRoute('/m/nope', '')).toEqual({ tab: 'library', view: 'albums' })
  })

  it('フォルダの名前に / や ? を含んでも往復できる', () => {
    const r: Route = { tab: 'folders', path: ['Anime', 'AC/DC', 'a?b #1'] }
    const u = new URL(formatRoute(r), 'http://x')
    expect(parseRoute(u.pathname, u.search)).toEqual(r)
    expect(formatRoute({ tab: 'folders', path: [] })).toBe('/m/folders')
  })

  it('不正な数値は無視する', () => {
    expect(parseRoute('/m/playlists/abc', '?edit=zzz')).toEqual({ tab: 'playlists' })
    expect(parseRoute('/m/library/albums', '?album=-1')).toEqual({ tab: 'library', view: 'albums' })
  })
})
