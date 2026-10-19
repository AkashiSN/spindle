// 購読のタブ。上に Inbox の未確認件数、購読ごとのカード（ジャケット・名前・最後の同期・同期ボタン・進み具合）。
// カードをタップすると詳細（SubscriptionDetail。route は /m/subscriptions/{id}）。
// - 同期はサーバで続く。進み具合は GET .../progress で取り、SSE の job イベント（表示中の購読のジョブか、
//   走行中の購読があるとき）で 250ms 間引いて取り直す。タブに戻ったとき・画面が前面に戻ったとき・再接続でも取り直す
// - 「完了」は投入した ytdl がすべて終わった時点（subscriptions.ts の syncPhase）
// - ytmusic が無効（一覧が 404 / 503）なら短い文言だけ出す

import { useCallback, useEffect, useRef, useState } from 'react'
import { ApiError } from '../../api/client'
import type { AlbumRow, Subscription } from '../../api/types'
import { TrailingDebounce } from '../../lib/debounce'
import {
  getSubscriptionProgress,
  inboxSummary,
  listAlbums,
  listSubscriptions,
  syncSubscription,
  type SubscriptionProgress,
} from '../api'
import type { Route } from '../route'
import { cardSummary, syncButton, syncedAgo, syncPhase } from '../subscriptions'
import { useEventSubscription } from '../useEvents'
import { SubscriptionDetail } from './SubscriptionDetail'
import { SubscriptionArt, SyncProgressBar } from './SubscriptionParts'

type SubscriptionsRoute = Extract<Route, { tab: 'subscriptions' }>

type ListState = { kind: 'loading' } | { kind: 'disabled' } | { kind: 'error'; message: string } | { kind: 'ok'; items: Subscription[] }

const nowSecs = () => Math.floor(Date.now() / 1000)

function errorText(e: unknown): string {
  return e instanceof Error ? e.message : String(e)
}

export function SubscriptionsTab({
  route,
  navigate,
}: {
  route: SubscriptionsRoute
  navigate: (r: Route, opts?: { replace?: boolean }) => void
}) {
  const [list, setList] = useState<ListState>({ kind: 'loading' })
  const [progress, setProgress] = useState<Record<number, SubscriptionProgress>>({})
  const [pending, setPending] = useState<number | null>(null)
  const [artwork, setArtwork] = useState<Map<number, string>>(new Map())
  // 同期ボタンを押して応答待ちの購読（二度押しを防ぐ）と、押した結果のエラー
  const [busy, setBusy] = useState<Set<number>>(new Set())
  const busyRef = useRef(new Set<number>())
  const [syncError, setSyncError] = useState<Record<number, string>>({})
  const [now, setNow] = useState(nowSecs)

  // 最新の一覧と進み具合（SSE のハンドラから見る）
  const latest = useRef<{ ids: number[]; progress: Record<number, SubscriptionProgress> }>({ ids: [], progress: {} })
  useEffect(() => {
    latest.current = { ids: list.kind === 'ok' ? list.items.map((s) => s.id) : [], progress }
  })

  const loadProgress = useCallback(async (id: number) => {
    try {
      const p = await getSubscriptionProgress(id)
      setProgress((m) => ({ ...m, [id]: p }))
    } catch {
      // 一覧の取り直しで消えた購読など。次の取り直しに任せる
    }
  }, [])

  /** 一覧・各購読の進み具合・Inbox の件数を取り直す */
  const load = useCallback(() => {
    inboxSummary()
      .then((s) => setPending(s?.pending ?? null))
      .catch(() => setPending(null))
    listSubscriptions()
      .then((items) => {
        setNow(nowSecs())
        setList({ kind: 'ok', items })
        for (const s of items) void loadProgress(s.id)
      })
      .catch((e: unknown) => {
        if (e instanceof ApiError && (e.status === 404 || e.status === 503)) setList({ kind: 'disabled' })
        else setList({ kind: 'error', message: `読み込みに失敗しました（${errorText(e)}）` })
      })
  }, [loadProgress])

  useEffect(() => load(), [load])

  // ジャケットは album の artwork_hash（一覧が届いてから 1 回だけ引く）
  const needArtwork = list.kind === 'ok' && list.items.some((s) => s.album_id != null)
  useEffect(() => {
    if (!needArtwork) return
    let alive = true
    listAlbums()
      .then((albums: AlbumRow[]) => {
        if (!alive) return
        const m = new Map<number, string>()
        for (const a of albums) if (a.artwork_hash != null) m.set(a.id, a.artwork_hash)
        setArtwork(m)
      })
      .catch(() => {})
    return () => {
      alive = false
    }
  }, [needArtwork])

  const [refetch] = useState(() => new TrailingDebounce(() => load(), 250, 1000))
  useEffect(() => () => refetch.cancel(), [refetch])
  useEventSubscription({
    onJob: (e) => {
      const { ids, progress: pm } = latest.current
      const hit = ids.some((id) => {
        const p = pm[id]
        if (!p) return false
        return p.sync?.id === e.id || p.items.some((j) => j.id === e.id) || syncPhase(p) === 'running'
      })
      if (hit) refetch.trigger()
    },
    onOpen: (reconnect) => {
      if (reconnect) refetch.trigger()
    },
  })
  // 画面が前面に戻ったら取り直す（閉じている間も同期はサーバで進んでいる）
  useEffect(() => {
    const onVis = () => {
      if (document.visibilityState === 'visible') refetch.trigger()
    }
    document.addEventListener('visibilitychange', onVis)
    return () => document.removeEventListener('visibilitychange', onVis)
  }, [refetch])

  const sync = useCallback(
    async (id: number) => {
      // 二度押しは state の反映を待たずに ref で弾く
      if (busyRef.current.has(id)) return
      busyRef.current.add(id)
      setBusy(new Set(busyRef.current))
      setSyncError((m) => {
        const rest = { ...m }
        delete rest[id]
        return rest
      })
      try {
        // started / already のどちらでも、走っている同期の進み具合を出す
        await syncSubscription(id)
      } catch (e) {
        setSyncError((m) => ({ ...m, [id]: `同期を始められませんでした（${errorText(e)}）` }))
      }
      await loadProgress(id)
      busyRef.current.delete(id)
      setBusy(new Set(busyRef.current))
    },
    [loadProgress],
  )

  const items = list.kind === 'ok' ? list.items : null
  const id = route.subscriptionId
  const current = id != null && items ? (items.find((s) => s.id === id) ?? null) : null
  const gone = id != null && items != null && current == null
  useEffect(() => {
    if (gone) navigate({ tab: 'subscriptions' }, { replace: true })
  }, [gone, navigate])

  if (list.kind === 'disabled') return <p className="m-placeholder">YouTube の取り込みは無効です（PC 版の設定で有効にできます）</p>
  if (list.kind === 'error')
    return (
      <div className="m-center-row">
        <p className="m-error">{list.message}</p>
        <button type="button" onClick={load}>
          再読み込み
        </button>
      </div>
    )
  if (items == null) return <p className="m-placeholder">読み込み中…</p>

  if (id != null) {
    if (current == null) return <p className="m-placeholder">読み込み中…</p>
    return (
      <SubscriptionDetail
        subscription={current}
        progress={progress[current.id] ?? null}
        busy={busy.has(current.id)}
        error={syncError[current.id] ?? null}
        now={now}
        artworkHash={current.album_id != null ? (artwork.get(current.album_id) ?? null) : null}
        onSync={() => void sync(current.id)}
        onBack={() => navigate({ tab: 'subscriptions' })}
      />
    )
  }

  return (
    <div className="m-library">
      <div className="m-scroll">
        {pending != null && pending > 0 && (
          <p className="m-notice">Inbox に未確認が {pending} 件あります（確認は PC で）</p>
        )}
        {items.length === 0 && <p className="m-placeholder">購読はまだありません（追加は PC 版で）</p>}
        <ul className="m-list">
          {items.map((s) => (
            <li key={s.id}>
              <SubscriptionCard
                subscription={s}
                progress={progress[s.id] ?? null}
                busy={busy.has(s.id)}
                error={syncError[s.id] ?? null}
                now={now}
                artworkHash={s.album_id != null ? (artwork.get(s.album_id) ?? null) : null}
                onOpen={() => navigate({ tab: 'subscriptions', subscriptionId: s.id })}
                onSync={() => void sync(s.id)}
              />
            </li>
          ))}
        </ul>
      </div>
    </div>
  )
}

function SubscriptionCard({
  subscription: s,
  progress,
  busy,
  error,
  now,
  artworkHash,
  onOpen,
  onSync,
}: {
  subscription: Subscription
  progress: SubscriptionProgress | null
  busy: boolean
  error: string | null
  now: number
  artworkHash: string | null
  onOpen: () => void
  onSync: () => void
}) {
  const phase = progress ? syncPhase(progress) : null
  const btn = syncButton(phase, busy)
  // 未同期は「最後の同期: 未同期」で足りるので 1 行は出さない
  const summary = progress && !(progress.sync == null && s.last_result == null) ? cardSummary(s, progress) : ''
  const summaryClass = phase === 'failed' ? 'm-sub-summary m-error-text' : phase === 'done' ? 'm-sub-summary m-ok-text' : 'm-sub-summary'
  return (
    <div className="m-sub-card">
      <div className="m-sub-top">
        <button type="button" className="m-row m-sub-open" onClick={onOpen}>
          <SubscriptionArt hash={artworkHash} />
          <span className="m-row-main">
            <span className="m-row-title">
              {s.albumartist} / {s.album}
            </span>
            <span className="m-row-sub">最後の同期: {syncedAgo(s.last_synced_at, now)}</span>
          </span>
        </button>
        <button type="button" className="m-sub-sync" disabled={btn.disabled} onClick={onSync}>
          {btn.label}
        </button>
      </div>
      {phase === 'running' && progress && <SyncProgressBar progress={progress} />}
      {summary !== '' && <p className={summaryClass}>{summary}</p>}
      {error != null && <p className="m-error m-sub-summary">{error}</p>}
    </div>
  )
}
