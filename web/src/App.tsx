// 画面の骨格（SPEC §12.1、D-58: ヘッダ → プレイヤーバー → 左（ツリー / アルバムアート）・右
// （プロパティ領域 / 表））とアプリ全体の状態。
//
// - 認証: GET /api/auth/session が 401 ならログイン画面。API の 401 でもログイン画面へ戻す
// - 表の集合 = サイドバーの scope + 上部ナビの検索語（filter）+ ソート
// - 選択は lib/selection の immutable な値。表示フィルタ・ソート・SSE では変えない
// - SSE は一覧より先に開く（D-36）。library で表示中ページを取り直し、job / batch で下部バーを更新

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import { apiFetch, apiPost, onUnauthorized } from './api/client'
import type { LibraryEvent, Playlist, TrackRow } from './api/types'
import { AlbumGrid } from './components/AlbumGrid'
import { AlbumArt } from './components/AlbumArt'
import { HistoryView } from './components/HistoryView'
import { JobsView } from './components/JobsView'
import { Login } from './components/Login'
import { Placeholder } from './components/Placeholder'
import { SettingsView } from './components/SettingsView'
import { PlayerBar } from './components/PlayerBar'
import { RightPanel, type SelectionSummary } from './components/RightPanel'
import { SmartRuleEditor, type RuleDraft } from './components/SmartRuleEditor'
import { Sidebar, type Scope } from './components/Sidebar'
import { TopNav } from './components/TopNav'
import { TrackTable } from './components/TrackTable'
import { useAlbums } from './hooks/useAlbums'
import { useBatchEdit } from './hooks/useBatchEdit'
import { useDragSize } from './hooks/useDragSize'
import { useEvents } from './hooks/useEvents'
import { useHistory } from './hooks/useHistory'
import { useJobSummary } from './hooks/useJobSummary'
import { useOperations } from './hooks/useOperations'
import { usePlayer } from './hooks/usePlayer'
import { usePlaylists } from './hooks/usePlaylists'
import { useSettings } from './hooks/useSettings'
import { useTrackDetails } from './hooks/useTrackDetails'
import { useTracks } from './hooks/useTracks'
import { PendingCounter, type PendingCount } from './lib/pendingCount'
import { scopeAfterPlaylistDelete, sortForScope } from './lib/playlists'
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
  // プロパティタブの詳細キャッシュの世代。タグやファイルが変わり得るイベントで進める
  const [detailsVersion, setDetailsVersion] = useState(0)
  const bumpDetails = useCallback(() => setDetailsVersion((v) => v + 1), [])

  // ルール編集中は表を編集中の DSL（WHERE）で差し替える（P1-7）。空なら scope のまま
  const [ruleDraft, setRuleDraft] = useState<RuleDraft | null>(null)
  const [draftDsl, setDraftDsl] = useState('')
  const draftTimer = useRef<number | null>(null)
  const filter: Filter = useMemo(
    () => (ruleDraft && draftDsl ? { dsl: draftDsl, q: query || undefined } : { ...scope, q: query || undefined }),
    [scope, query, ruleDraft, draftDsl],
  )
  const filterParam = filterToParam(filter)

  // 一覧は SSE を開いてから取る（開く前のイベントを失わない。D-36）
  const tracks = useTracks(filter, sort, sseOpen)
  const albums = useAlbums(sseOpen)
  const playlists = usePlaylists(sseOpen)
  const refreshPlaylists = playlists.refresh
  const jobs = useJobSummary(sseOpen)
  // 履歴は画面を開いたときに取り、開いている間は batch イベントで取り直す
  const history = useHistory(sseOpen && view === 'history')
  /** ジョブ / 設定画面から「バッチ #n」で飛んできたときに開くバッチ */
  const [historyFocus, setHistoryFocus] = useState<number | null>(null)
  const openBatch = useCallback((id: number) => {
    setHistoryFocus(id)
    setView('history')
  }, [])
  const settings = useSettings(sseOpen && view === 'settings')
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
      // missing の件数が変わる
      refreshPlaylists()
      // 選択集合内の行が変わったかは client で分からないので、library は常に数え直す
      refreshPending()
      bumpDetails()
      const keep = Math.max(visibleEnd.current + 1, 1)
      if (e.kind === 'bulk') {
        tracks.reload(keep)
        return
      }
      // ids: 表示中（読み込み済み）に含まれる id があるときだけ取り直す
      if (e.track_ids.some((id) => tracks.byId.has(id))) tracks.reload(keep)
    },
    [albums, refreshPlaylists, tracks, refreshPending, bumpDetails],
  )
  /** 表示範囲・ジョブ要約・アルバム・反映待ち集計をまとめて取り直す（再接続 / resync） */
  const refreshAll = useCallback(() => {
    jobs.refresh()
    albums.refresh()
    refreshPlaylists()
    tracks.reload(Math.max(visibleEnd.current + 1, 1))
    refreshPending()
    bumpDetails()
    if (view === 'history') history.refresh()
  }, [jobs, albums, refreshPlaylists, tracks, refreshPending, bumpDetails, view, history])
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
  // ジョブの完了は行の値（RG / FLAC 検査 / Derived のバッジ、プロパティの詳細）を変えるが、
  // `library` イベントは scan だけが流す（SPEC §9）。done / failed のたびに表示ページを取り直すと
  // 一括の transcode で数千回になるので 3 秒に 1 回に間引く（D-58）
  const rowRefreshTimer = useRef<number | null>(null)
  const scheduleRowRefresh = useCallback(() => {
    if (rowRefreshTimer.current != null) return
    rowRefreshTimer.current = window.setTimeout(() => {
      rowRefreshTimer.current = null
      tracks.reload(Math.max(visibleEnd.current + 1, 1))
      bumpDetails()
    }, 3000)
  }, [tracks, bumpDetails])
  useEffect(
    () => () => {
      if (rowRefreshTimer.current != null) window.clearTimeout(rowRefreshTimer.current)
    },
    [],
  )
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
    onJob: (e) => {
      jobs.refresh()
      if (e.state === 'done' || e.state === 'failed') scheduleRowRefresh()
    },
    onBatch: () => {
      jobs.refresh()
      // バッチの状態変化は反映待ち / conflict バッジと「うち反映待ち」を変える
      tracks.reload(Math.max(visibleEnd.current + 1, 1))
      refreshPending()
      bumpDetails()
      if (view === 'history') history.refresh()
    },
    onLibrary,
    onResync: refreshAll,
    onPlaylist: (e) => {
      // 再評価で項目が変わった: 一覧の件数と、表示中ならその表を取り直す
      refreshPlaylists()
      if (scope.playlist_id != null && e.playlist_ids.includes(scope.playlist_id)) {
        tracks.reload(Math.max(visibleEnd.current + 1, 1))
      }
    },
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

  // プロパティタブの詳細（選択行の先頭 50 件）
  const selectedIds = useMemo(() => selectedRows.map((r) => r.id), [selectedRows])
  const details = useTrackDetails(selectedIds, detailsVersion, sseOpen && view === 'tracks')

  // ---------------------------------------------------------------- 一括編集

  const edit = useBatchEdit(selection, sortToParam(sort))
  const operations = useOperations(selection, sortToParam(sort))
  const handleInlineEdit = useCallback(
    (id: number, columnId: string, value: string) => edit.applyInline(id, columnId, value),
    [edit],
  )

  // ---------------------------------------------------------------- 再生

  const player = usePlayer(tracks.snapshot.rows, tracks.snapshot.exhausted, tracks.ensure)

  // ---------------------------------------------------------------- 表示

  const handleSort = useCallback((key: SortKey) => setSort((s) => toggleSort(s, key)), [])
  const handleRange = useCallback((end: number) => {
    visibleEnd.current = end
  }, [])
  const handleScope = useCallback(
    (s: Scope) => {
      // プレイリストに入ったら position 順、出たら既定ソートへ（lib/playlists）
      setSort((sort) => sortForScope(scope, s, sort))
      setScope(s)
      setView('tracks')
    },
    [scope],
  )

  // ---------------------------------------------------------------- プレイリスト（P1-6）

  const [playlistNotice, setPlaylistNotice] = useState<string | null>(null)
  const currentPlaylist = useMemo(
    () => (scope.playlist_id == null ? null : (playlists.items.find((p) => p.id === scope.playlist_id) ?? null)),
    [scope.playlist_id, playlists.items],
  )
  const playlistName = (id: number) => playlists.items.find((p) => p.id === id)?.name ?? `#${id}`
  const reloadIfCurrent = useCallback(
    (playlistId: number) => {
      if (scope.playlist_id === playlistId) tracks.reload(Math.max(visibleEnd.current + 1, 1))
    },
    [scope.playlist_id, tracks],
  )
  const failNotice = (e: unknown) => setPlaylistNotice(`失敗: ${e instanceof Error ? e.message : String(e)}`)
  const openRuleEditor = (p: Playlist | null) => {
    setRuleDraft(p ? { id: p.id, name: p.name, rule: p.rule_source ?? '' } : { id: null, name: '', rule: '' })
    setDraftDsl(p?.rule_source ?? '')
    setView('tracks')
  }
  const closeRuleEditor = () => {
    setRuleDraft(null)
    setDraftDsl('')
  }
  const handleDraft = (d: RuleDraft) => {
    setRuleDraft(d)
    // 表への反映は 250ms 遅らせる（検索語と同じ）。不正な DSL は 400 で表が空になるだけ
    if (draftTimer.current != null) window.clearTimeout(draftTimer.current)
    draftTimer.current = window.setTimeout(() => setDraftDsl(d.rule.trim()), 250)
  }
  const addToPlaylist = async (playlistId: number, what: Selection | number[]) => {
    try {
      const r = await playlists.addTracks(playlistId, what, sortToParam(sort))
      if (!r) return
      setPlaylistNotice(
        `「${playlistName(playlistId)}」に ${r.added} 件を追加${r.skipped > 0 ? `（${r.skipped} 件は既に入っている）` : ''}`,
      )
      reloadIfCurrent(playlistId)
    } catch (e) {
      failNotice(e)
    }
  }
  const removeSelectedFromPlaylist = async () => {
    if (!currentPlaylist || selection.kind === 'none') return
    try {
      const r = await playlists.removeTracks(currentPlaylist.id, selection)
      if (!r) return
      setPlaylistNotice(`「${currentPlaylist.name}」から ${r.removed} 件を除外`)
      clearSelection()
      reloadIfCurrent(currentPlaylist.id)
    } catch (e) {
      failNotice(e)
    }
  }
  const reorderPlaylist = async (trackIds: number[], before: number | null) => {
    if (!currentPlaylist) return
    try {
      await playlists.moveTracks(currentPlaylist.id, trackIds, before)
      reloadIfCurrent(currentPlaylist.id)
    } catch (e) {
      failNotice(e)
    }
  }
  const logout = async () => {
    try {
      await apiPost('/api/auth/logout', {})
    } finally {
      onLogout()
    }
  }

  // 左右の境界（左カラムの幅）と、左カラム内のアルバムアートの高さ
  const sideDrag = useDragSize({
    key: 'layout.sidebar',
    cssVar: '--sidebar-w',
    fallback: 340,
    min: 200,
    max: 800,
    axis: 'x',
    direction: 1,
  })
  const artDrag = useDragSize({
    key: 'layout.art',
    cssVar: '--art-h',
    fallback: 320,
    min: 0,
    max: 800,
    axis: 'y',
    direction: -1,
  })
  // アルバムアート: 選択行（先頭）のアルバム、無ければ再生中のアルバム
  const artAlbum = useMemo(() => {
    const albumId = selectedRows[0]?.album_id ?? player.track?.album_id ?? null
    return albumId == null ? null : (albums.albums.find((a) => a.id === albumId) ?? null)
  }, [selectedRows, player.track, albums.albums])

  return (
    <div className="shell">
      <TopNav
        view={view}
        onView={setView}
        query={query}
        onQuery={setQuery}
        summary={jobs.summary}
        connected={connected}
        onLogout={logout}
      />
      <PlayerBar player={player} />
      <div className="left-col">
      <Sidebar
        albums={albums.albums}
        scope={scope}
        onScope={handleScope}
        playlists={playlists}
        onPlaylistDeleted={(id) => {
          // 表示中のプレイリストを消したら「すべて」へ（行キャッシュと position ソートを残さない）
          const next = scopeAfterPlaylistDelete(scope, id)
          if (next !== scope) handleScope(next)
        }}
        onDropTracks={(id, ids) => void addToPlaylist(id, ids)}
        onEditRule={openRuleEditor}
        onRefreshed={reloadIfCurrent}
        playlistNotice={playlistNotice}
        onPlaylistNotice={setPlaylistNotice}
      />
      <div className="divider-h" onMouseDown={artDrag.onMouseDown} title="ドラッグで高さを変更" />
      <AlbumArt album={artAlbum} />
      </div>
      <div className="divider-v" onMouseDown={sideDrag.onMouseDown} title="ドラッグで幅を変更" />
      <div className="right-col">
      {view === 'tracks' && (
      <RightPanel
        selection={selection}
        summary={summary}
        selectedRows={selectedRows}
        details={details}
        edit={edit}
        ops={operations}
        playlists={playlists.items}
        onAddToPlaylist={(id) => void addToPlaylist(id, selection)}
      />
      )}
      <main className="center">
        {view === 'tracks' && ruleDraft && (
          <SmartRuleEditor
            draft={ruleDraft}
            playlists={playlists}
            onDraft={handleDraft}
            onSaved={(id) => {
              closeRuleEditor()
              setPlaylistNotice('スマートプレイリストを保存して評価した')
              handleScope({ playlist_id: id })
            }}
            onCancel={closeRuleEditor}
          />
        )}
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
            onPlay={player.play}
            playingId={player.track?.id ?? null}
            playlist={
              currentPlaylist && currentPlaylist.kind === 'manual' && !ruleDraft
                ? {
                    name: currentPlaylist.name,
                    reorderable: sort.key === 'position' && !sort.desc,
                    onRemoveSelected: () => void removeSelectedFromPlaylist(),
                    onReorder: (ids, before) => void reorderPlaylist(ids, before),
                  }
                : null
            }
          />
        ) : view === 'albums' ? (
          <AlbumGrid
            albums={albums.albums}
            error={albums.error}
            onOpen={(a) => handleScope({ album_id: a.id })}
          />
        ) : view === 'cd' ? (
          <Placeholder title="CD" note="リッピングのウィザードは P2" />
        ) : view === 'jobs' ? (
          <JobsView
            jobs={jobs}
            onOpenBatch={openBatch}
          />
        ) : view === 'history' ? (
          <HistoryView history={history} focusId={historyFocus} />
        ) : (
          <SettingsView settings={settings} onOpenBatch={openBatch} />
        )}
      </main>
      </div>
    </div>
  )
}
