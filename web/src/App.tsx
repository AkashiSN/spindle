// 3 ペイン骨格（SPEC §12.1）とアプリ全体の状態。
//
// - 認証: GET /api/auth/session が 401 ならログイン画面。API の 401 でもログイン画面へ戻す
// - 表の集合 = サイドバーの scope + 上部ナビの検索語（filter）+ ソート
// - 選択は lib/selection の immutable な値。表示フィルタ・ソート・SSE では変えない
// - SSE は一覧より先に開く（D-36）。library で表示中ページを取り直し、job / batch で下部バーを更新

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { apiFetch, apiPost, onUnauthorized } from './api/client'
import type { LibraryEvent, TrackRow } from './api/types'
import { BottomBar } from './components/BottomBar'
import { HistoryView } from './components/HistoryView'
import { Login } from './components/Login'
import { Placeholder } from './components/Placeholder'
import { RightPanel, type SelectionSummary } from './components/RightPanel'
import { Sidebar, type Scope } from './components/Sidebar'
import { TopNav } from './components/TopNav'
import { TrackTable } from './components/TrackTable'
import { useAlbums } from './hooks/useAlbums'
import { useBatchEdit } from './hooks/useBatchEdit'
import { useEvents } from './hooks/useEvents'
import { useHistory } from './hooks/useHistory'
import { useJobSummary } from './hooks/useJobSummary'
import { useTracks } from './hooks/useTracks'
import { PendingCounter, type PendingCount } from './lib/pendingCount'
import type { View } from './lib/views'
import {
  DEFAULT_SORT,
  filterToParam,
  sortToParam,
  toggleSort,
  type Filter,
  type Sort,
  type SortKey,
} from './lib/filter'
import {
  clickRow,
  isSelected,
  NO_SELECTION,
  selectAll,
  selectionCount,
  settleFilterTotal,
  type ClickModifiers,
  type Selection,
  type VisibleOrder,
} from './lib/selection'

type AuthState = 'checking' | 'in' | 'out'

export default function App() {
  const [auth, setAuth] = useState<AuthState>('checking')
  useEffect(() => {
    apiFetch('/api/auth/session')
      .then(() => setAuth('in'))
      .catch(() => setAuth('out'))
  }, [])
  useEffect(() => onUnauthorized(() => setAuth('out')), [])

  if (auth === 'checking') return <main className="login muted">確認中…</main>
  if (auth === 'out') return <Login onLoggedIn={() => setAuth('in')} />
  return <Shell onLogout={() => setAuth('out')} />
}

function Shell({ onLogout }: { onLogout: () => void }) {
  const [view, setView] = useState<View>('tracks')
  const [scope, setScope] = useState<Scope>({})
  const [query, setQuery] = useState('')
  const [sort, setSort] = useState<Sort>(DEFAULT_SORT)
  const [selection, setSelection] = useState<Selection>(NO_SELECTION)
  /** filter 形の選択時にサーバが返していた total（選択集合の件数の基準） */
  const [selectionTotal, setSelectionTotal] = useState<number | null>(null)
  /** filter 形の選択の「うち反映待ち」。選択時のフィルタ文字列をキーに持つ（lib/pendingCount） */
  const [pendingCount, setPendingCount] = useState<PendingCount | null>(null)
  const [pendingCounter] = useState(
    () =>
      new PendingCounter(
        (url, signal) => apiFetch<{ total: number }>(url, { signal }),
        setPendingCount,
      ),
  )
  const [sseOpen, setSseOpen] = useState(false)
  const [connected, setConnected] = useState(false)

  const filter: Filter = useMemo(() => ({ ...scope, q: query || undefined }), [scope, query])
  const filterParam = filterToParam(filter)

  // 一覧は SSE を開いてから取る（開く前のイベントを失わない。D-36）
  const tracks = useTracks(filter, sort, sseOpen)
  const albums = useAlbums(sseOpen)
  const jobs = useJobSummary(sseOpen)
  // 履歴は画面を開いたときに取り、開いている間は batch イベントで取り直す
  const history = useHistory(sseOpen && view === 'history')
  const visibleEnd = useRef(0)

  // filter 形の選択の「うち反映待ち」。選択集合は immutable でも中の行の pending はバッチの進行で
  // 変わるので、選択時に加えて batch / library / resync / 再接続でも数え直す（世代管理は counter 側）
  const selectionFilterKey = selection.kind === 'filter' ? selection.filter : null
  const refreshPending = useCallback(() => {
    if (selectionFilterKey != null) pendingCounter.refresh(selectionFilterKey)
  }, [pendingCounter, selectionFilterKey])
  useEffect(() => {
    if (selectionFilterKey == null) pendingCounter.clear()
    else pendingCounter.refresh(selectionFilterKey)
  }, [pendingCounter, selectionFilterKey])

  const onLibrary = useCallback(
    (e: LibraryEvent) => {
      albums.refresh()
      // 選択集合内の行が変わったかは client で分からないので、library は常に数え直す
      refreshPending()
      const keep = Math.max(visibleEnd.current + 1, 1)
      if (e.kind === 'bulk') {
        tracks.reload(keep)
        return
      }
      // ids: 表示中（読み込み済み）に含まれる id があるときだけ取り直す
      if (e.track_ids.some((id) => tracks.byId.has(id))) tracks.reload(keep)
    },
    [albums, tracks, refreshPending],
  )
  /** 表示範囲・ジョブ要約・アルバム・反映待ち集計をまとめて取り直す（再接続 / resync） */
  const refreshAll = useCallback(() => {
    jobs.refresh()
    albums.refresh()
    tracks.reload(Math.max(visibleEnd.current + 1, 1))
    refreshPending()
    if (view === 'history') history.refresh()
  }, [jobs, albums, tracks, refreshPending, view, history])
  // SSE が切れたとき（401 で閉じられた場合を含む）にセッションを確かめる。401 なら
  // apiFetch の onUnauthorized 経由でログイン画面へ戻る。連続するエラーは 5 秒に 1 回に間引くが、
  // 最後のエラーは必ず確認する（サーバ再起動直後は接続拒否 → 再接続で 401 の順に来る。
  // 401 の EventSource は再接続しないので、そこで捨てると二度と気付けない）
  const lastAuthCheck = useRef(0)
  const authCheckTimer = useRef<number | null>(null)
  const checkSession = useCallback(() => {
    const run = () => {
      lastAuthCheck.current = Date.now()
      authCheckTimer.current = null
      apiFetch('/api/auth/session').catch(() => {})
    }
    const wait = 5000 - (Date.now() - lastAuthCheck.current)
    if (wait <= 0) run()
    else if (authCheckTimer.current == null) authCheckTimer.current = window.setTimeout(run, wait)
  }, [])
  useEvents({
    onOpen: (reconnect) => {
      setSseOpen(true)
      setConnected(true)
      // 切断中に流れた job / batch / library は再送されない（D-40）。開き直したら全部取り直す
      if (reconnect) refreshAll()
    },
    onError: () => {
      setConnected(false)
      checkSession()
    },
    onJob: () => jobs.refresh(),
    onBatch: () => {
      jobs.refresh()
      // バッチの状態変化は反映待ち / conflict バッジと「うち反映待ち」を変える
      tracks.reload(Math.max(visibleEnd.current + 1, 1))
      refreshPending()
      if (view === 'history') history.refresh()
    },
    onLibrary,
    onResync: refreshAll,
  })

  // ---------------------------------------------------------------- 選択

  const handleRowClick = useCallback(
    (id: number, mods: ClickModifiers, order: VisibleOrder) => {
      // filter 形を持ったまま表示フィルタを変えた後の Ctrl / Shift は無視される（lib/selection）
      setSelection((prev) => clickRow(prev, id, mods, order, filterParam))
    },
    [filterParam],
  )
  const handleSelectAll = useCallback(() => {
    setSelection(selectAll(filterParam))
    // 1 ページ目が届く前なら null。同じフィルタの total が届いた時点で下の effect が一度だけ確定する
    setSelectionTotal(tracks.snapshot.total)
  }, [filterParam, tracks.snapshot.total])
  // Ctrl+A 時に total が無かった場合、同じフィルタの total が届いた描画で一度だけ確定する
  // （描画中の setState は「前回描画の情報を保持する」React の作法。effect 経由より 1 描画早い）
  const settled = settleFilterTotal(selection, selectionTotal, filterParam, tracks.snapshot.total)
  if (settled != null && selectionTotal == null) setSelectionTotal(settled)
  const clearSelection = useCallback(() => {
    setSelection(NO_SELECTION)
    setSelectionTotal(null)
  }, [])

  const pendingInFilter =
    selectionFilterKey != null && pendingCount?.key === selectionFilterKey ? pendingCount.total : null

  const selectedRows: TrackRow[] = useMemo(() => {
    if (selection.kind === 'none') return []
    if (selection.kind === 'ids') {
      const out: TrackRow[] = []
      for (const id of selection.ids) {
        const r = tracks.byId.get(id)
        if (r) out.push(r)
      }
      return out
    }
    // filter 形: 表示フィルタが同じときだけ読み込み済み行から拾う
    if (selection.filter !== filterParam) return []
    return tracks.snapshot.rows.filter((r) => isSelected(selection, r.id))
  }, [selection, tracks.byId, tracks.snapshot.rows, filterParam])

  const summary: SelectionSummary = useMemo(() => {
    const count = selectionCount(selection, selectionTotal)
    if (selection.kind === 'ids') {
      return { count, pending: selectedRows.filter((r) => r.pending_batch_id != null).length }
    }
    if (selection.kind === 'filter') {
      // 除外した行のうち反映待ちだったものは差し引く（読み込み済みで判定できる分だけ）
      let excludedPending = 0
      for (const id of selection.excludeIds) {
        if (tracks.byId.get(id)?.pending_batch_id != null) excludedPending++
      }
      return {
        count,
        pending: pendingInFilter == null ? null : Math.max(0, pendingInFilter - excludedPending),
      }
    }
    return { count: 0, pending: 0 }
  }, [selection, selectionTotal, selectedRows, pendingInFilter, tracks.byId])

  // ---------------------------------------------------------------- 一括編集

  const edit = useBatchEdit(selection, sortToParam(sort))
  const handleInlineEdit = useCallback(
    (id: number, columnId: string, value: string) => edit.applyInline(id, columnId, value),
    [edit],
  )

  // ---------------------------------------------------------------- 表示

  const handleSort = useCallback((key: SortKey) => setSort((s) => toggleSort(s, key)), [])
  const handleRange = useCallback((end: number) => {
    visibleEnd.current = end
  }, [])
  const handleScope = useCallback((s: Scope) => {
    setScope(s)
    setView('tracks')
  }, [])
  const logout = async () => {
    try {
      await apiPost('/api/auth/logout', {})
    } finally {
      onLogout()
    }
  }

  return (
    <div className="shell">
      <TopNav view={view} onView={setView} query={query} onQuery={setQuery} onLogout={logout} />
      <Sidebar albums={albums.albums} scope={scope} onScope={handleScope} />
      <main className="center">
        {view === 'tracks' ? (
          <TrackTable
            rows={tracks.snapshot.rows}
            total={tracks.snapshot.total}
            loading={tracks.snapshot.loading}
            error={tracks.snapshot.error}
            ensure={tracks.ensure}
            sort={sort}
            onSort={handleSort}
            selection={selection}
            highlightFilterSelection={selection.kind === 'filter' && selection.filter === filterParam}
            onRowClick={handleRowClick}
            onSelectAll={handleSelectAll}
            onClearSelection={clearSelection}
            onRangeChange={handleRange}
            preview={edit.preview}
            onInlineEdit={handleInlineEdit}
          />
        ) : view === 'albums' ? (
          <Placeholder title="アルバム" note="サムネイルグリッドは P1。ツリーのアルバムをクリックすると一覧が絞られる" />
        ) : view === 'cd' ? (
          <Placeholder title="CD" note="リッピングのウィザードは P2" />
        ) : view === 'jobs' ? (
          <Placeholder title="ジョブ" note="種別ごとの待ち行列・進捗・再試行は後続タスクで。下部バーの要約は SSE で更新中" />
        ) : view === 'history' ? (
          <HistoryView history={history} />
        ) : (
          <Placeholder title="設定" note="config.toml の閲覧、再スキャン / deep scan / GC dry-run は後続タスクで" />
        )}
      </main>
      <RightPanel selection={selection} summary={summary} selectedRows={selectedRows} edit={edit} />
      <BottomBar summary={jobs.summary} connected={connected} onJobsClick={() => setView('jobs')} />
    </div>
  )
}
