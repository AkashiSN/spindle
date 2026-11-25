import { describe, expect, it } from 'vitest'
import type { UploadGroupView, UploadMemberView, UploadOriginView, UploadView } from '../api/types'
import {
  ACCEPT_EXTS,
  CHUNK_BYTES,
  canDiscard,
  canPublish,
  canResend,
  completedOrigin,
  chunkRanges,
  clientPath,
  fileTogglePatch,
  groupLabel,
  groupTogglePatch,
  mergeSelected,
  originState,
  publishedDirs,
  publishLabel,
  resumeBanner,
  resumeCandidates,
  serialQueue,
  shouldSend,
  statusLabel,
  statusTone,
  toggleFileIn,
  toggleGroupIn,
  unpackPending,
  uploadErrorMessage,
} from './upload'
import type { UploadPatch } from '../api/types'

const MiB = 1024 * 1024

function member(over: Partial<UploadMemberView> = {}): UploadMemberView {
  return {
    staged: 'files/o1',
    display: 'X/01.flac',
    publish_name: '01.flac',
    renamed: false,
    included: true,
    library_match: null,
    ...over,
  }
}

function group(over: Partial<UploadGroupView> = {}): UploadGroupView {
  return {
    key: 'tag:a\u001fx',
    albumartist: 'A',
    album: 'X',
    included: true,
    selectable: true,
    track_count: 1,
    format: 'FLAC 96k/24',
    status: { kind: 'new' },
    statuses: [{ kind: 'new' }],
    tracks: [member()],
    companions: [],
    ...over,
  }
}

function origin(over: Partial<UploadOriginView> = {}): UploadOriginView {
  return {
    id: 'o1',
    client_path: 'X/01.flac',
    class: 'audio',
    total: 10,
    received: 10,
    complete: true,
    removed: false,
    damaged: false,
    zip: null,
    ...over,
  }
}

function view(over: Partial<UploadView> = {}): UploadView {
  return {
    id: '0123456789abcdef',
    created_at: 1_700_000_000,
    updated_at: 1_700_000_000,
    expires_at: 1_700_000_000 + 30 * 86400,
    origins: [origin()],
    busy: false,
    groups: [group()],
    ignored: [],
    unassigned_companions: [],
    publish: null,
    publish_requested_at: null,
    publish_error: null,
    discarded_at: null,
    choices: { merges: [], excluded_files: [], excluded_groups: [], included_files: [], included_groups: [] },
    ...over,
  }
}

describe('定数', () => {
  it('チャンクは 32 MiB、受ける拡張子は音声 7 種と zip', () => {
    expect(CHUNK_BYTES).toBe(32 * MiB)
    expect(ACCEPT_EXTS).toEqual(['flac', 'wav', 'm4a', 'aif', 'aiff', 'mp3', 'opus', 'zip'])
  })
})

describe('chunkRanges', () => {
  it('70 MiB は 3 区間で、最後は 64 MiB から 70 MiB', () => {
    const r = chunkRanges(70 * MiB)
    expect(r).toHaveLength(3)
    expect(r[0]).toEqual([0, 32 * MiB])
    expect(r[1]).toEqual([32 * MiB, 64 * MiB])
    expect(r[2]).toEqual([64 * MiB, 70 * MiB])
  })
  it('ちょうど割り切れるときに空の区間を足さない', () => {
    expect(chunkRanges(64 * MiB)).toEqual([
      [0, 32 * MiB],
      [32 * MiB, 64 * MiB],
    ])
  })
  it('0 バイトは空の区間 1 つ（空の PUT で完了させる）', () => {
    expect(chunkRanges(0)).toEqual([[0, 0]])
  })
  it('区間の大きさを渡せる', () => {
    expect(chunkRanges(5, 2)).toEqual([
      [0, 2],
      [2, 4],
      [4, 5],
    ])
  })
})

describe('clientPath', () => {
  const f = (name: string, rel = '') => {
    const file = new File(['x'], name)
    Object.defineProperty(file, 'webkitRelativePath', { value: rel })
    return file
  }
  it('ドロップの相対パス → webkitRelativePath → 名前の順', () => {
    expect(clientPath(f('01.flac', 'Album/01.flac'), 'Drop/01.flac')).toBe('Drop/01.flac')
    expect(clientPath(f('01.flac', 'Album/01.flac'))).toBe('Album/01.flac')
    expect(clientPath(f('01.flac'))).toBe('01.flac')
    expect(clientPath(f('01.flac'), '')).toBe('01.flac')
  })
})

describe('shouldSend', () => {
  it('隠しファイル（.DS_Store、._ の AppleDouble）は送らない', () => {
    expect(shouldSend('A/.DS_Store')).toBe(false)
    expect(shouldSend('A/._01.flac')).toBe(false)
    expect(shouldSend('A/01.flac')).toBe(true)
    expect(shouldSend('readme.txt')).toBe(true)
  })
})

describe('statusLabel / statusTone', () => {
  it('5 種の文言と色', () => {
    expect(statusLabel({ kind: 'library_duplicate', paths: ['a'] })).toBe('同じ音声がライブラリにある')
    expect(statusLabel({ kind: 'inbox_exists', dir: 'upload/A/X' })).toBe('Inbox に同じ件がある')
    expect(statusLabel({ kind: 'append_to', album: 'Album W', track_count: 10 })).toBe('ライブラリの『Album W』（10 曲）に追加')
    expect(statusLabel({ kind: 'needs_name' })).toBe('Inbox で名前を入れる')
    expect(statusLabel({ kind: 'new' })).toBe('新規')
    expect(statusTone({ kind: 'library_duplicate', paths: [] })).toBe('ng')
    expect(statusTone({ kind: 'inbox_exists', dir: '' })).toBe('warn')
    expect(statusTone({ kind: 'append_to', album: '', track_count: 0 })).toBe('info')
    expect(statusTone({ kind: 'needs_name' })).toBe('warn')
    expect(statusTone({ kind: 'new' })).toBe('ok')
  })
})

describe('groupLabel', () => {
  it('アルバム名があれば アーティスト — アルバム、無ければ置き場所の手がかり', () => {
    expect(groupLabel(group())).toBe('A — X')
    expect(groupLabel(group({ album: null, tracks: [member({ display: 'mora.zip/misc/a.mp3' })] }))).toBe(
      'アルバム名なし（mora.zip/misc）',
    )
    expect(groupLabel(group({ album: null, tracks: [member({ display: 'a.mp3' })] }))).toBe('アルバム名なし（a.mp3）')
  })
})

describe('publishLabel', () => {
  it('チェックを外した件は数えない', () => {
    const v = view({
      groups: [
        group({ key: 'a', track_count: 2 }),
        group({ key: 'b', track_count: 3 }),
        group({ key: 'c', track_count: 5, included: false }),
      ],
    })
    expect(publishLabel(v)).toBe('2 件 5 曲を Inbox へ置く')
  })
  it('凍結した後は計画の件で数える', () => {
    const v = view({
      groups: [],
      publish: {
        frozen_at: 1,
        groups: [
          { key: 'a', albumartist: 'A', album: 'X', track_count: 4, companion_count: 1, base_dest: 'upload/A/X', stage: 'planned' },
        ],
      },
    })
    expect(publishLabel(v)).toBe('1 件 4 曲を Inbox へ置く')
  })
})

describe('canPublish', () => {
  it('置ける', () => {
    expect(canPublish(view(), false)).toEqual({ ok: true, reason: null })
  })
  it('送信中', () => {
    expect(canPublish(view(), true)).toEqual({ ok: false, reason: '送信中' })
  })
  it('zip を展開中（展開中・未展開・ジョブが動いている）', () => {
    const zip = (z: Partial<NonNullable<UploadOriginView['zip']>>) =>
      origin({ class: 'zip', zip: { generation: 0, unpacking: false, rejected: [], error: null, ...z } })
    expect(canPublish(view({ origins: [zip({ unpacking: true })] }), false).reason).toBe('zip を展開中')
    expect(canPublish(view({ origins: [zip({})] }), false).reason).toBe('zip を展開中')
    expect(canPublish(view({ busy: true }), false).reason).toBe('zip を展開中')
    // 外した zip は数えない
    expect(canPublish(view({ origins: [origin(), { ...zip({ unpacking: true }), removed: true }] }), false).ok).toBe(true)
  })
  it('展開できなかった zip がある', () => {
    const o = origin({ class: 'zip', zip: { generation: 0, unpacking: false, rejected: [], error: { reason: '壊れている', retryable: false } } })
    expect(canPublish(view({ origins: [o] }), false).reason).toBe('展開できなかった zip がある')
  })
  it('送信が途中で止まったファイルがある（外した・壊れたものは数えない）', () => {
    expect(canPublish(view({ origins: [origin({ complete: false, received: 3 })] }), false).reason).toBe(
      '送信が途中で止まったファイルがある',
    )
    expect(canPublish(view({ origins: [origin(), origin({ id: 'o2', complete: false, removed: true })] }), false).ok).toBe(true)
    expect(canPublish(view({ origins: [origin(), origin({ id: 'o2', complete: false, damaged: true })] }), false).ok).toBe(true)
  })
  it('置く件が無い', () => {
    expect(canPublish(view({ groups: [] }), false).reason).toBe('置く件が無い')
    expect(canPublish(view({ groups: [group({ included: false })] }), false).reason).toBe('置く件が無い')
  })
  it('曲の無い件がある', () => {
    expect(canPublish(view({ groups: [group(), group({ key: 'b', track_count: 0 })] }), false).reason).toBe('曲の無い件がある')
  })
  it('公開を要求した後は押せない。止まった失敗があれば押し直せる', () => {
    const frozen = view({ publish_requested_at: 5, publish: { frozen_at: 5, groups: [] } })
    expect(canPublish(frozen, false).ok).toBe(false)
    expect(canPublish({ ...frozen, publish_error: '置けない' }, false).ok).toBe(true)
    expect(canPublish({ ...frozen, publish_error: '置けない', busy: true }, false).ok).toBe(false)
  })
})

describe('canDiscard', () => {
  it('busy でも公開要求済みでもなければ捨てられる', () => {
    expect(canDiscard(view())).toBe(true)
    expect(canDiscard(view({ busy: true }))).toBe(false)
    expect(canDiscard(view({ publish_requested_at: 1 }))).toBe(false)
  })
  it('公開が止まった失敗があり、ジョブが動いていなければ捨てられる', () => {
    expect(canDiscard(view({ publish_requested_at: 1, publish_error: 'x' }))).toBe(true)
    expect(canDiscard(view({ publish_requested_at: 1, publish_error: 'x', busy: true }))).toBe(false)
  })
})

describe('mergeSelected', () => {
  it('表の順で今の merges の後ろに足す（先頭の件の名前になる）', () => {
    const v = view({
      groups: [group({ key: 'a' }), group({ key: 'b' }), group({ key: 'c' })],
      choices: { merges: [['x', 'y']], excluded_files: [], excluded_groups: [], included_files: [], included_groups: [] },
    })
    expect(mergeSelected(v, ['c', 'a'])).toEqual([
      ['x', 'y'],
      ['a', 'c'],
    ])
  })
  it('2 件未満なら変えない。表に無いキーは落とす', () => {
    const v = view({ groups: [group({ key: 'a' }), group({ key: 'b' })] })
    expect(mergeSelected(v, ['a'])).toEqual([])
    expect(mergeSelected(v, ['a', 'zz'])).toEqual([])
  })
})

describe('fileTogglePatch / groupTogglePatch', () => {
  const choices = { merges: [], excluded_files: [], excluded_groups: [], included_files: [], included_groups: [] }
  it('既定で入るファイルを外すと excluded_files に足す', () => {
    expect(fileTogglePatch(choices, member({ staged: 'f1' }), true)).toEqual({ excluded_files: ['f1'], included_files: [] })
  })
  it('外したファイルを戻すと excluded_files から消す', () => {
    const c = { ...choices, excluded_files: ['f1', 'f2'] }
    expect(fileTogglePatch(c, member({ staged: 'f1', included: false }), true)).toEqual({ excluded_files: ['f2'], included_files: [] })
  })
  it('ライブラリに同じ音声がある曲（既定で外れる）を入れると included_files に足す', () => {
    const m = member({ staged: 'f1', included: false, library_match: 'A/X/01.flac' })
    expect(fileTogglePatch(choices, m, true)).toEqual({ excluded_files: [], included_files: ['f1'] })
  })
  it('同梱ファイルは既定で入る', () => {
    const m = member({ staged: 'c1', included: false })
    expect(fileTogglePatch({ ...choices, excluded_files: ['c1'] }, m, false)).toEqual({ excluded_files: [], included_files: [] })
  })
  it('件: 既定で入る件を外す / 同じ音声の件を入れる', () => {
    expect(groupTogglePatch(choices, group({ key: 'g' }))).toEqual({ excluded_groups: ['g'], included_groups: [] })
    const dup = group({ key: 'g', included: false, status: { kind: 'library_duplicate', paths: [] }, statuses: [{ kind: 'library_duplicate', paths: [] }] })
    expect(groupTogglePatch(choices, dup)).toEqual({ excluded_groups: [], included_groups: ['g'] })
  })
})

describe('originState', () => {
  it('送信済み・途中で止まった・壊れた', () => {
    expect(originState(origin())).toEqual({ label: '送信済み', tone: 'ok', action: null })
    expect(originState(origin({ complete: false }))).toEqual({ label: '送信が途中で止まった', tone: 'warn', action: 'remove' })
    expect(originState(origin({ damaged: true }))).toEqual({ label: '壊れた。外して送り直す', tone: 'ng', action: 'remove' })
  })
  it('zip の展開の状態', () => {
    const z = (zip: Partial<NonNullable<UploadOriginView['zip']>>) =>
      origin({ class: 'zip', zip: { generation: 0, unpacking: false, rejected: [], error: null, ...zip } })
    expect(originState(z({ unpacking: true })).label).toBe('展開中')
    expect(originState(z({})).label).toBe('展開待ち')
    expect(originState(z({ generation: 1 })).label).toBe('展開済み')
    expect(originState(z({ error: { reason: '空きが足りない', retryable: true } }))).toEqual({
      label: '展開できなかった: 空きが足りない',
      tone: 'ng',
      action: 'unpack',
    })
    expect(originState(z({ error: { reason: '暗号化', retryable: false } })).action).toBe('remove')
  })
})

describe('publishedDirs', () => {
  it('置き終えた件の置き場所だけ', () => {
    const v = view({
      publish: {
        frozen_at: 1,
        groups: [
          { key: 'a', albumartist: 'A', album: 'X', track_count: 1, companion_count: 0, base_dest: 'upload/A/X', stage: 'published', dest: 'upload/A/X (2)' },
          { key: 'b', albumartist: 'B', album: 'Y', track_count: 1, companion_count: 0, base_dest: 'upload/B/Y', stage: 'publishing', dest: 'upload/B/Y' },
        ],
      },
    })
    expect(publishedDirs(v)).toEqual([{ key: 'a', label: 'A — X', dir: 'upload/A/X (2)', tracks: 1 }])
  })
})

describe('resumeBanner / resumeCandidates', () => {
  it('無ければ null', () => {
    expect(resumeBanner([])).toBeNull()
  })
  it('日時と件・曲の数', () => {
    const t = new Date(2026, 10, 24, 22, 10).getTime() / 1000
    const v = view({
      created_at: t,
      groups: [group({ key: 'a' }), group({ key: 'b', track_count: 1 }), group({ key: 'c', track_count: 1 }), group({ key: 'd', included: false })],
    })
    expect(resumeBanner([v])).toBe('前回の送信で Inbox へまだ置いていないものがあります（11/24 22:10、3 件 3 曲）')
  })
  it('新しい順に並べ、送ったものが何も無いものは出さない', () => {
    const a = view({ id: 'a', created_at: 10 })
    const b = view({ id: 'b', created_at: 20 })
    const empty = view({ id: 'e', created_at: 30, origins: [], groups: [] })
    expect(resumeCandidates([a, b, empty]).map((v) => v.id)).toEqual(['b', 'a'])
  })
})

describe('uploadErrorMessage', () => {
  it('分かっている失敗は日本語に、知らないものは本文の message', () => {
    expect(uploadErrorMessage(409, { error: 'busy' })).toBe('展開か公開が動いている。終わるまで待つ')
    expect(uploadErrorMessage(409, { error: 'incomplete' })).toBe('送信か展開が終わっていないものがある')
    expect(uploadErrorMessage(409, { error: 'empty' })).toBe('置く件が無い')
    expect(uploadErrorMessage(413, { error: 'limit', message: '1 ファイルの上限' })).toBe('上限を超える: 1 ファイルの上限')
    expect(uploadErrorMessage(413, null)).toBe('1 回に送れる大きさを超えた（HTTP 413）')
    expect(uploadErrorMessage(500, { error: 'x', message: 'y' })).toBe('y')
  })
})

describe('serialQueue + toggleFileIn / toggleGroupIn', () => {
  // サーバの PATCH の模擬: 与えた項目を置き換え、included をマニフェストから決め直す
  function server(v: UploadView, body: UploadPatch): UploadView {
    const choices = { ...v.choices, ...body }
    const groups = v.groups.map((g) => ({
      ...g,
      included: !choices.excluded_groups.includes(g.key),
      tracks: g.tracks.map((t) => ({
        ...t,
        included: !choices.excluded_files.includes(t.staged) && (t.library_match == null || choices.included_files.includes(t.staged)),
      })),
    }))
    return { ...v, choices, groups }
  }

  it('続けて押した 2 つの切り替えが両方残る（本文は前の応答の後の表示から作る）', async () => {
    let current = view({
      groups: [group({ key: 'g', tracks: [member({ staged: 'f1' }), member({ staged: 'f2' })], track_count: 2 })],
    })
    const run = serialQueue()
    const patch = (build: (v: UploadView) => UploadPatch | null) =>
      run(async () => {
        const body = build(current)
        if (body == null) return
        await new Promise((r) => setTimeout(r, 5))
        current = server(current, body)
      })
    await Promise.all([patch((v) => toggleFileIn(v, 'f1')), patch((v) => toggleFileIn(v, 'f2')), patch((v) => toggleGroupIn(v, 'g'))])
    expect(current.choices.excluded_files).toEqual(['f1', 'f2'])
    expect(current.choices.excluded_groups).toEqual(['g'])
  })

  it('同じファイルを 2 度押すと元に戻る', async () => {
    let current = view({ groups: [group({ tracks: [member({ staged: 'f1' })] })] })
    const run = serialQueue()
    const toggle = () =>
      run(async () => {
        const body = toggleFileIn(current, 'f1')
        if (body != null) current = server(current, body)
      })
    await Promise.all([toggle(), toggle()])
    expect(current.choices.excluded_files).toEqual([])
    expect(current.groups[0]?.tracks[0]?.included).toBe(true)
  })

  it('失敗した仕事の後も次を走らせる', async () => {
    const run = serialQueue()
    const order: number[] = []
    const a = run(async () => {
      order.push(1)
      throw new Error('x')
    })
    const b = run(async () => {
      order.push(2)
    })
    await expect(a).rejects.toThrow('x')
    await b
    expect(order).toEqual([1, 2])
  })

  it('見つからない・チェックできない件は null', () => {
    const v = view({ groups: [group({ key: 'g', selectable: false })] })
    expect(toggleFileIn(v, 'nope')).toBeNull()
    expect(toggleGroupIn(v, 'g')).toBeNull()
    expect(toggleGroupIn(v, 'nope')).toBeNull()
  })
})

describe('canResend / completedOrigin / unpackPending', () => {
  it('外した・壊れた origin のファイルは足し直せる', () => {
    const v = view({ origins: [origin({ id: 'o1', removed: true }), origin({ id: 'o2', damaged: true }), origin({ id: 'o3' })] })
    expect(canResend({ originId: 'o1' }, v)).toBe(true)
    expect(canResend({ originId: 'o2' }, v)).toBe(true)
    expect(canResend({ originId: 'o3' }, v)).toBe(false)
    expect(canResend({ originId: null }, v)).toBe(false)
  })
  it('受け取り終えた同じパスの origin', () => {
    const v = view({ origins: [origin({ id: 'o1', client_path: 'a.flac', removed: true }), origin({ id: 'o2', client_path: 'a.flac' })] })
    expect(completedOrigin(v, 'a.flac')?.id).toBe('o2')
    expect(completedOrigin(v, 'b.flac')).toBeNull()
    expect(completedOrigin(view({ origins: [origin({ client_path: 'a.flac', complete: false })] }), 'a.flac')).toBeNull()
  })
  it('展開待ち・展開中の zip', () => {
    const z = origin({ class: 'zip', zip: { generation: 0, unpacking: true, rejected: [], error: null } })
    expect(unpackPending(view({ origins: [z] }))).toBe(true)
    expect(unpackPending(view({ origins: [{ ...z, zip: { generation: 1, unpacking: false, rejected: [], error: null } }] }))).toBe(false)
  })
})
