// アップロード画面の状態（SPEC §12.6「アップロード」、§7.12「送信」）。
// 開いているアップロード 1 つと、その送信キュー（同時 2、32 MiB ずつ PUT）、前回の続きの一覧。
// 送信は画面を切り替えても続く（hook は App が持つ）。取り直しは SSE の job イベントと、画面を開いていて
// 何かが動いている間の 2 秒ごと

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { apiFetch, parseErrorBody, putBinary } from '../api/client'
import type { UploadList, UploadOriginView, UploadPatch, UploadView } from '../api/types'
import {
  canResend,
  chunkRanges,
  clientPath,
  completedOrigin,
  inProgress,
  mergeSelected,
  mutationQueue,
  type MutationQueue,
  PARALLEL_SENDS,
  shouldSend,
  uploadErrorMessage,
  viewSequence,
} from '../lib/upload'

/** 送信キューの 1 項（このタブで選んだファイル。ブラウザを閉じると消える） */
export interface SendItem {
  /** キューの中の識別子 */
  n: number
  path: string
  file: File
  total: number
  sent: number
  originId: string | null
  state: 'queued' | 'sending' | 'done' | 'error'
  error: string | null
}

export interface UploadState {
  /** 開いているアップロード（まだ何も送っていなければ null） */
  view: UploadView | null
  /** 前回の続き（開いているもの以外）と壊れたもの。まだ取れていなければ null */
  list: UploadList | null
  sends: SendItem[]
  sending: boolean
  /** 開いているアップロードへの変更（③・公開・捨てるなど）を送っている途中（終わるまで公開・切り替えを押せない） */
  mutating: boolean
  error: string | null
  clearError: () => void
  /** ファイル（`path` はドロップで辿った相対パス）を足して送り始める */
  addFiles: (files: Array<{ file: File; path?: string }>) => void
  /** ドロップを受ける（フォルダは再帰して相対パスを作る） */
  addDrop: (dt: DataTransfer) => void
  /** 失敗したファイルを最初から送り直す */
  resend: (n: number) => void
  /** 前回の続きを開く */
  open: (id: string) => void
  /** 開いているものを閉じて、新しく送れるようにする */
  startNew: () => void
  refresh: () => void
  /** ③ の変更。本文は前の変更の応答を反映した後の表示から作る（PATCH は項目を置き換えるので、他の変更と 1 列にする） */
  patch: (build: (v: UploadView) => UploadPatch | null) => Promise<void>
  merge: (keys: string[]) => Promise<void>
  publish: () => Promise<void>
  discard: (id?: string) => Promise<boolean>
  discardBroken: (dir: string) => Promise<void>
  removeOrigin: (originId: string) => Promise<void>
  retryUnpack: (originId: string) => Promise<void>
}

const POLL_MS = 2000

type Fail = { status: number; body: unknown }

/** JSON の要求。本文が JSON でない失敗（axum の 413 は text/plain）は body = null */
const request = (path: string, init: RequestInit) => parseErrorBody<unknown>(path, init)

/** 1 チャンクの PUT（生の本文。401 はクライアントがログイン画面へ切り替える） */
async function putChunk(path: string, blob: Blob, signal: AbortSignal): Promise<{ ok: true; body: { received: number; complete: boolean } } | ({ ok: false } & Fail)> {
  const r = await putBinary(path, blob, signal)
  if (r.ok) return { ok: true, body: r.body as { received: number; complete: boolean } }
  return r
}

/** ドロップの項目を辿ってファイルと相対パスを集める（`webkitGetAsEntry` は同期で呼ぶ必要がある） */
function entriesOf(dt: DataTransfer): { entries: FileSystemEntry[]; loose: File[] } {
  const entries: FileSystemEntry[] = []
  const loose: File[] = []
  for (const item of Array.from(dt.items)) {
    if (item.kind !== 'file') continue
    const e = item.webkitGetAsEntry()
    if (e != null) entries.push(e)
    else {
      const f = item.getAsFile()
      if (f != null) loose.push(f)
    }
  }
  return { entries, loose }
}

async function walkEntry(e: FileSystemEntry, out: Array<{ file: File; path?: string }>): Promise<void> {
  const path = e.fullPath.replace(/^\/+/, '')
  if (e.isFile) {
    const file = await new Promise<File>((ok, ng) => (e as FileSystemFileEntry).file(ok, ng))
    out.push({ file, path })
    return
  }
  if (!e.isDirectory) return
  const reader = (e as FileSystemDirectoryEntry).createReader()
  // readEntries は 1 回に全部を返すとは限らない。空が返るまで読む
  for (;;) {
    const batch = await new Promise<FileSystemEntry[]>((ok, ng) => reader.readEntries(ok, ng))
    if (batch.length === 0) break
    for (const child of batch) await walkEntry(child, out)
  }
}

/** 失敗の本文のコード */
const r409 = (r: { status: number; body: unknown }) =>
  r.status === 409 ? ((r.body as { error?: string } | null)?.error ?? null) : null

const errText = (e: unknown) => (e instanceof Error ? e.message : String(e))

/** `enabled` はアップロード画面を表示中か（表示中だけ一覧を取り、周期で取り直す） */
export function useUpload(enabled: boolean): UploadState {
  const [view, setView] = useState<UploadView | null>(null)
  const [list, setList] = useState<UploadList | null>(null)
  const [error, setError] = useState<string | null>(null)
  // 開いているアップロードへの変更を積んで、まだ終わっていない数
  const [pendingMutations, setPendingMutations] = useState(0)
  // 送信キューは送信の処理が読み書きするので ref に持ち、描画には写しを渡す
  const sendsRef = useRef<SendItem[]>([])
  const [sends, setSends] = useState<SendItem[]>([])
  const bump = useCallback(() => setSends(sendsRef.current.map((x) => ({ ...x }))), [])
  // 開いているアップロードの id（送信の処理は ref を、描画は state を読む）
  const idRef = useRef<string | null>(null)
  const [currentId, setCurrentId] = useState<string | null>(null)
  // 送信は画面を閉じても（hook が消えても）走り続けないよう、外れたら止める
  const abortRef = useRef<AbortController>(new AbortController())
  useEffect(() => {
    const c = new AbortController()
    abortRef.current = c
    return () => c.abort()
  }, [])
  const setId = useCallback((id: string | null) => {
    idRef.current = id
    setCurrentId(id)
  }, [])
  // 開いているアップロードへの変更（③・公開・捨てるなど）を 1 列に送る。対象は積んだときの id
  // （描画中に ref を読まないよう、最初に使うときに作る）
  const mutationsRef = useRef<MutationQueue | null>(null)
  const mutations = useCallback((): MutationQueue => {
    if (mutationsRef.current == null) mutationsRef.current = mutationQueue(idRef, setPendingMutations)
    return mutationsRef.current
  }, [])
  const viewRef = useRef<UploadView | null>(null)
  const nextN = useRef(1)
  const active = useRef(0)

  const setCurrent = useCallback((v: UploadView | null) => {
    viewRef.current = v
    setView(v)
  }, [])
  // 取り直しの応答の順序: 変更の応答を反映した後に、それより前に送った GET の応答で戻さない
  const seqRef = useRef(viewSequence())
  /** アップロード `id` への変更の応答（の表示）を反映する。もう開いているものでなければ捨てる */
  const applyMutation = useCallback(
    (id: string, v?: UploadView) => {
      if (idRef.current !== id || (v !== undefined && v.id !== id)) return
      seqRef.current.mutated()
      if (v !== undefined) setCurrent(v)
    },
    [setCurrent],
  )

  const fetchList = useCallback(() => {
    apiFetch<UploadList>('/api/uploads')
      .then(setList)
      .catch((e: unknown) => setError(errText(e)))
  }, [])

  const fetchView = useCallback(async () => {
    const id = idRef.current
    if (id == null) return
    // 周期・送信の後から投げっぱなしで呼ぶので、通信の失敗もここで文言にする
    const seq = seqRef.current.send()
    const r = await request(`/api/uploads/${id}`, { method: 'GET' }).catch((e: unknown) => {
      setError(errText(e))
      return null
    })
    if (r == null || idRef.current !== id) return
    if (r.ok) {
      // 変更の応答より古い（か、後に送った GET が先に届いた）応答は捨てる
      if (!seqRef.current.accept(seq)) return
      setCurrent(r.body as UploadView)
    } else if (r.status === 404) {
      // 捨てた・GC が消した
      setId(null)
      setCurrent(null)
    } else {
      setError(uploadErrorMessage(r.status, r.body))
    }
  }, [setCurrent, setId])

  const refreshTimer = useRef<number | null>(null)
  const refresh = useCallback(() => {
    if (refreshTimer.current != null) return
    refreshTimer.current = window.setTimeout(() => {
      refreshTimer.current = null
      void fetchView()
      if (enabled) fetchList()
    }, 250)
  }, [enabled, fetchView, fetchList])

  useEffect(() => {
    if (!enabled) return
    fetchList()
    void fetchView()
  }, [enabled, fetchList, fetchView])

  const sending = sends.some((s) => s.state === 'queued' || s.state === 'sending')

  // 画面を開いていて何かが動いている間は 2 秒ごとに取り直す
  const moving = sending || (view != null && inProgress(view))
  useEffect(() => {
    if (!enabled || !moving) return
    const t = window.setInterval(() => void fetchView(), POLL_MS)
    return () => window.clearInterval(t)
  }, [enabled, moving, fetchView])

  // 送信中はブラウザの離脱確認を出す（途中のファイルは送り直しになる）
  useEffect(() => {
    if (!sending) return
    const h = (e: BeforeUnloadEvent) => {
      e.preventDefault()
      e.returnValue = ''
    }
    window.addEventListener('beforeunload', h)
    return () => window.removeEventListener('beforeunload', h)
  }, [sending])

  const update = useCallback(
    (n: number, patch: Partial<SendItem>) => {
      const s = sendsRef.current.find((x) => x.n === n)
      if (s != null) Object.assign(s, patch)
      bump()
    },
    [bump],
  )

  /** 1 ファイルを送る: origin を作り（同じパスの未完の origin があればそれ）、最初から順に PUT */
  const sendOne = useCallback(
    async (s: SendItem, id: string) => {
      update(s.n, { state: 'sending', sent: 0, error: null })
      const signal = abortRef.current.signal
      const b = await request(`/api/uploads/${id}/origins`, {
        method: 'POST',
        body: JSON.stringify({ path: s.path, total: s.total }),
        signal,
      })
      if (!b.ok) {
        if (r409(b) === 'exists') {
          // 最後のチャンクの応答を取りこぼして「再送」した: サーバに受け取り終えたものがあれば済み
          await fetchView()
          const done = completedOrigin(viewRef.current, s.path)
          if (done != null) {
            update(s.n, { state: 'done', sent: s.total, originId: done.id })
            return
          }
        }
        update(s.n, { state: 'error', error: uploadErrorMessage(b.status, b.body) })
        return
      }
      const origin = (b.body as { origin: UploadOriginView }).origin
      update(s.n, { originId: origin.id })
      let offset = 0
      // 進まない応答が続いたら諦める（offset の食い違いで戻るのは 1 回は普通に起きる）
      let stalls = 0
      for (;;) {
        const [rs, re] = chunkRanges(s.total - offset)[0] ?? [0, 0]
        const start = offset + rs
        const end = offset + re
        const r = await putChunk(`/api/uploads/${id}/origins/${origin.id}?offset=${start}`, s.file.slice(start, end), signal)
        const before = offset
        if (r.ok) {
          if (r.body.complete) {
            update(s.n, { state: 'done', sent: s.total })
            return
          }
          // received == total で未完なら、次の周で offset = total の空の PUT が完了させる
          offset = r.body.received
        } else {
          const body = (r.body ?? {}) as { error?: string; received?: number }
          if (r.status !== 409 || body.error !== 'offset' || typeof body.received !== 'number') {
            update(s.n, { state: 'error', error: uploadErrorMessage(r.status, r.body) })
            return
          }
          // サーバの .part の大きさが正。そこから続ける
          offset = body.received
        }
        stalls = offset > before ? 0 : stalls + 1
        if (stalls > 3) {
          update(s.n, { state: 'error', error: '送信が進まない' })
          return
        }
        update(s.n, { sent: offset })
      }
    },
    [update, fetchView],
  )

  /** キューから同時 2 つまで送る（送り終えるたびに次を取る。自分を呼ぶので ref 経由） */
  const pumpRef = useRef<() => void>(() => {})
  const pump = useCallback(() => {
    const id = idRef.current
    if (id == null) return
    while (active.current < PARALLEL_SENDS) {
      const s = sendsRef.current.find((x) => x.state === 'queued')
      if (s == null) break
      active.current++
      s.state = 'sending'
      sendOne(s, id)
        .catch((e: unknown) => update(s.n, { state: 'error', error: `送れなかった: ${errText(e)}` }))
        .finally(() => {
          active.current--
          void fetchView()
          pumpRef.current()
        })
    }
    bump()
  }, [sendOne, update, fetchView, bump])
  useEffect(() => {
    pumpRef.current = pump
  }, [pump])

  /** 送り先のアップロードを用意する（無い・公開を要求済みなら新しく作る） */
  const ensureUpload = useCallback(async (): Promise<string | null> => {
    const cur = viewRef.current
    if (idRef.current != null && (cur == null || cur.publish_requested_at == null)) return idRef.current
    const r = await request('/api/uploads', { method: 'POST' })
    if (!r.ok) {
      setError(uploadErrorMessage(r.status, r.body))
      return null
    }
    const v = r.body as UploadView
    sendsRef.current = []
    setId(v.id)
    applyMutation(v.id, v)
    return v.id
  }, [applyMutation, setId])

  const creating = useRef<Promise<string | null> | null>(null)
  const addFiles = useCallback(
    (files: Array<{ file: File; path?: string }>) => {
      const picked = files
        .map(({ file, path }) => ({ file, path: clientPath(file, path) }))
        .filter((f) => shouldSend(f.path))
      if (picked.length === 0) return
      setError(null)
      if (creating.current == null) creating.current = ensureUpload().finally(() => (creating.current = null))
      void creating.current.then((id) => {
        if (id == null) return
        for (const { file, path } of picked) {
          // 同じパスを送っている・送り終えたものは足さない（失敗したものは送り直す）
          const same = sendsRef.current.find((s) => s.path === path)
          if (same != null && same.state !== 'error' && !canResend(same, viewRef.current)) continue
          if (same != null) {
            Object.assign(same, { file, total: file.size, sent: 0, originId: null, state: 'queued', error: null })
            continue
          }
          sendsRef.current.push({
            n: nextN.current++,
            path,
            file,
            total: file.size,
            sent: 0,
            originId: null,
            state: 'queued',
            error: null,
          })
        }
        pump()
      })
    },
    [ensureUpload, pump],
  )

  const addDrop = useCallback(
    (dt: DataTransfer) => {
      const { entries, loose } = entriesOf(dt)
      void (async () => {
        const out: Array<{ file: File; path?: string }> = loose.map((file) => ({ file }))
        try {
          for (const e of entries) await walkEntry(e, out)
        } catch (e) {
          setError(`ドロップしたものを読めない: ${errText(e)}`)
        }
        addFiles(out)
      })()
    },
    [addFiles],
  )

  const resend = useCallback(
    (n: number) => {
      const s = sendsRef.current.find((x) => x.n === n)
      if (s == null || s.state !== 'error') return
      Object.assign(s, { state: 'queued', sent: 0, error: null })
      pump()
    },
    [pump],
  )

  const open = useCallback(
    (id: string) => {
      if (sendsRef.current.some((s) => s.state === 'queued' || s.state === 'sending')) {
        setError('送信中は別のアップロードを開けない')
        return
      }
      if (mutations().pending() > 0) {
        setError('変更を送っている間は別のアップロードを開けない')
        return
      }
      sendsRef.current = []
      bump()
      setId(id)
      setCurrent(null)
      setError(null)
      void fetchView()
      fetchList()
    },
    [mutations, bump, setId, setCurrent, fetchView, fetchList],
  )

  const startNew = useCallback(() => {
    if (sendsRef.current.some((s) => s.state === 'queued' || s.state === 'sending')) return
    if (mutations().pending() > 0) {
      setError('変更を送っている間は別に送り始められない')
      return
    }
    sendsRef.current = []
    bump()
    setId(null)
    setCurrent(null)
    fetchList()
  }, [mutations, bump, setId, setCurrent, fetchList])

  /** 変更系の共通（`id` は変更するアップロード）: 失敗は文言に、成功は取り直す */
  const mutate = useCallback(
    async (id: string, path: string, init: RequestInit, applyView = false): Promise<boolean> => {
      setError(null)
      try {
        const r = await request(path, init)
        if (!r.ok) {
          setError(uploadErrorMessage(r.status, r.body))
          await fetchView()
          return false
        }
        if (applyView && r.body != null) applyMutation(id, r.body as UploadView)
        else {
          applyMutation(id)
          await fetchView()
        }
        return true
      } catch (e) {
        setError(errText(e))
        return false
      }
    },
    [fetchView, applyMutation],
  )

  /** 開いているアップロードへの変更を 1 列に積む（開いていなければ何もしない） */
  const enqueue = useCallback(
    <T,>(task: (id: string) => Promise<T>): Promise<T | undefined> => {
      const id = idRef.current
      if (id == null) return Promise.resolve(undefined)
      return mutations().run(id, () => task(id))
    },
    [mutations],
  )

  const patch = useCallback(
    async (build: (v: UploadView) => UploadPatch | null) => {
      await enqueue(async (id) => {
        const v = viewRef.current
        if (v == null || v.id !== id) return
        const body = build(v)
        if (body == null) return
        await mutate(id, `/api/uploads/${id}`, { method: 'PATCH', body: JSON.stringify(body) }, true)
      })
    },
    [enqueue, mutate],
  )

  const merge = useCallback(
    async (keys: string[]) => {
      await patch((v) => ({ merges: mergeSelected(v, keys) }))
    },
    [patch],
  )

  // 先に積んだ ③ の変更の応答を反映してから送る（追い越すと古い選択のまま凍結される）。
  // 要求から取り直しまでは `mutating` が押せなくし、その後は表示（公開を要求済み）が押せなくする
  const publish = useCallback(async () => {
    await enqueue((id) => mutate(id, `/api/uploads/${id}/publish`, { method: 'POST' }))
  }, [enqueue, mutate])

  const discard = useCallback(
    async (target?: string) => {
      let ok: boolean
      if (target == null || target === idRef.current) {
        // 開いているもの: 先に積んだ変更の後に捨てる
        ok =
          (await enqueue(async (id) => {
            const done = await mutate(id, `/api/uploads/${id}`, { method: 'DELETE' })
            if (done && id === idRef.current) {
              sendsRef.current = []
              bump()
              setId(null)
              setCurrent(null)
            }
            return done
          })) ?? false
      } else {
        // 前回の続き（開いていないもの）は列に並べない
        ok = await mutate(target, `/api/uploads/${target}`, { method: 'DELETE' })
      }
      fetchList()
      return ok
    },
    [enqueue, mutate, bump, setId, setCurrent, fetchList],
  )

  const discardBroken = useCallback(
    async (dir: string) => {
      setError(null)
      try {
        const r = await request(`/api/uploads/broken/${encodeURIComponent(dir)}`, { method: 'DELETE' })
        if (!r.ok) {
          const code = (r.body as { error?: string } | null)?.error
          if (r.status === 409 && code === 'not_broken') {
            // マニフェストは読めるが表示を作れなかった: ふつうに捨てる
            await discard(dir.replace(/^\.spindle-upload-/, ''))
            return
          }
          setError(uploadErrorMessage(r.status, r.body))
        }
      } catch (e) {
        setError(errText(e))
      }
      fetchList()
    },
    [discard, fetchList],
  )

  const removeOrigin = useCallback(
    async (originId: string) => {
      await patch(() => ({ removed_origins: [originId] }))
    },
    [patch],
  )

  const retryUnpack = useCallback(
    async (originId: string) => {
      await enqueue((id) => mutate(id, `/api/uploads/${id}/origins/${originId}/unpack`, { method: 'POST' }))
    },
    [enqueue, mutate],
  )

  const clearError = useCallback(() => setError(null), [])

  // 一覧からは開いているものを除く（前回の続きの帯に出さない）
  const shownList = useMemo<UploadList | null>(() => {
    if (list == null) return null
    return { items: list.items.filter((v) => v.id !== currentId), broken: list.broken }
  }, [list, currentId])

  return {
    view,
    list: shownList,
    sends,
    sending,
    mutating: pendingMutations > 0,
    error,
    clearError,
    addFiles,
    addDrop,
    resend,
    open,
    startNew,
    refresh,
    patch,
    merge,
    publish,
    discard,
    discardBroken,
    removeOrigin,
    retryUnpack,
  }
}
