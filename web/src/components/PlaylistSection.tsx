// サイドバーのプレイリスト区画（SPEC §12.1、P1-6）。手動プレイリストの一覧・作成・改名・削除・
// 書き出し（プロファイル選択）・Playlists root からの取り込み。表の行をドラッグして落とすと追加。
// クリックは scope を `{ playlist_id }` に置き換えるだけ（表・列・選択の仕組みは共通）

import { useCallback, useEffect, useState, type DragEvent } from 'react'
import { ApiError } from '../api/client'
import { EXPORT_PROFILES, type ExportProfileName, type Fb2kQuery, type ImportCandidate, type Playlist } from '../api/types'
import type { Playlists } from '../hooks/usePlaylists'
import { formatDuration } from '../lib/format'
import { exportNotice, parseDragIds, TRACK_DRAG_TYPE } from '../lib/playlists'
import { Fb2kQueryDialog } from './Fb2kQueryDialog'

export function PlaylistSection({
  playlists,
  activeId,
  onOpen,
  onDeleted,
  onDropTracks,
  onEditRule,
  onRefreshed,
  notice,
  onNotice,
}: {
  playlists: Playlists
  /** scope が指しているプレイリスト */
  activeId: number | null
  onOpen: (id: number) => void
  /** 削除が終わったとき（表示中のプレイリストなら App が scope を戻す） */
  onDeleted: (id: number) => void
  /** 表からドラッグした行を落としたとき（manual だけ） */
  onDropTracks: (id: number, trackIds: number[]) => void
  /** スマート: 新規作成（id なし）/ ルール編集（id あり）を中央ペインで開く（P1-7） */
  onEditRule: (p: Playlist | null) => void
  /** 再評価が終わったとき（表示中なら表を取り直す） */
  onRefreshed: (id: number) => void
  /** 直近の操作結果（App が持つ。追加・除外は表側からも起きる） */
  notice: string | null
  onNotice: (text: string | null) => void
}) {
  const [menuFor, setMenuFor] = useState<number | null>(null)
  const [fb2k, setFb2k] = useState<{ name: string; result: Fb2kQuery } | null>(null)
  const [dropOver, setDropOver] = useState<number | null>(null)
  const [importOpen, setImportOpen] = useState(false)

  const fail = (e: unknown) => {
    const msg = e instanceof ApiError && e.code === 'duplicate' ? '同じ名前のプレイリストがある' : e instanceof Error ? e.message : String(e)
    onNotice(`失敗: ${msg}`)
  }

  const create = async () => {
    const name = window.prompt('プレイリスト名')?.trim()
    if (!name) return
    try {
      const p = await playlists.create(name)
      onNotice(`「${p.name}」を作成`)
      onOpen(p.id)
    } catch (e) {
      fail(e)
    }
  }
  const rename = async (p: Playlist) => {
    setMenuFor(null)
    const name = window.prompt('新しい名前', p.name)?.trim()
    if (!name || name === p.name) return
    try {
      await playlists.rename(p.id, name)
      onNotice(`「${p.name}」→「${name}」`)
    } catch (e) {
      fail(e)
    }
  }
  const remove = async (p: Playlist) => {
    setMenuFor(null)
    if (!window.confirm(`「${p.name}」を削除する？（ファイルは消えない。項目 ${p.track_count} 件の所属だけが消える）`)) return
    try {
      await playlists.remove(p.id)
      onNotice(`「${p.name}」を削除`)
      onDeleted(p.id)
    } catch (e) {
      fail(e)
    }
  }
  const exportTo = async (p: Playlist, profile: ExportProfileName) => {
    setMenuFor(null)
    try {
      const r = await playlists.exportTo(p.id, profile)
      onNotice(exportNotice(r))
    } catch (e) {
      fail(e)
    }
  }

  const acceptsDrop = (e: DragEvent, p: Playlist) =>
    p.kind === 'manual' && e.dataTransfer.types.includes(TRACK_DRAG_TYPE)
  const showFb2k = async (p: Playlist) => {
    setMenuFor(null)
    try {
      const result = await playlists.fb2kQuery(p.id)
      setFb2k({ name: p.name, result })
    } catch (e) {
      fail(e)
    }
  }
  const refreshSmart = async (p: Playlist) => {
    setMenuFor(null)
    try {
      const r = await playlists.refreshSmart(p.id)
      onNotice(`「${p.name}」を再評価: ${r.count} 件${r.changed ? '（変更あり）' : '（変更なし）'}`)
      if (r.changed) onRefreshed(p.id)
    } catch (e) {
      fail(e)
    }
  }

  return (
    <section className="playlists">
      <h2>
        プレイリスト
        <span className="spacer" />
        <button type="button" className="ghost" title="新しいプレイリスト" onClick={() => void create()}>
          ＋
        </button>
        <button type="button" className="ghost" title="新しいスマートプレイリスト（ルールで自動）" onClick={() => onEditRule(null)}>
          ＋⚙
        </button>
        <button type="button" className="ghost" title="Playlists フォルダの m3u8 を取り込む" onClick={() => setImportOpen((o) => !o)}>
          取り込み…
        </button>
      </h2>
      {playlists.error && <p className="error small">読み込みに失敗: {playlists.error}</p>}
      {importOpen && (
        <ImportPanel playlists={playlists} onClose={() => setImportOpen(false)} onNotice={onNotice} onOpen={onOpen} />
      )}
      <ul className="flat">
        {playlists.items.map((p) => (
          <li
            key={p.id}
            className={`playlist-row${dropOver === p.id ? ' drop-over' : ''}`}
            onDragOver={(e) => {
              if (!acceptsDrop(e, p)) return
              e.preventDefault()
              e.dataTransfer.dropEffect = 'copy'
              if (dropOver !== p.id) setDropOver(p.id)
            }}
            onDragLeave={() => setDropOver((cur) => (cur === p.id ? null : cur))}
            onDrop={(e) => {
              if (!acceptsDrop(e, p)) return
              e.preventDefault()
              setDropOver(null)
              const ids = parseDragIds(e.dataTransfer.getData(TRACK_DRAG_TYPE))
              if (ids) onDropTracks(p.id, ids)
            }}
          >
            <button
              type="button"
              className={`tree-node${activeId === p.id ? ' active' : ''}`}
              title={`${p.track_count} 件 / ${formatDuration(p.duration_ms)}${p.missing_count > 0 ? `（missing ${p.missing_count}）` : ''}${
                p.rule_source ? `\nルール: ${p.rule_source}` : ''
              }${p.exports.length > 0 ? `\n書き出し: ${p.exports.map((x) => x.out_path).join(', ')}` : ''}`}
              onClick={() => onOpen(p.id)}
            >
              {p.kind === 'smart' ? '⚙ ' : '♪ '}
              {p.name}
              <span className="muted"> {p.track_count}</span>
            </button>
            <button
              type="button"
              className="ghost row-menu"
              title="操作"
              aria-haspopup="menu"
              aria-expanded={menuFor === p.id}
              onClick={() => setMenuFor((cur) => (cur === p.id ? null : p.id))}
            >
              …
            </button>
            {menuFor === p.id && (
              <div className="popup-menu" role="menu">
                <button type="button" role="menuitem" onClick={() => void rename(p)}>
                  改名
                </button>
                {p.kind === 'smart' && (
                  <>
                    <button
                      type="button"
                      role="menuitem"
                      onClick={() => {
                        setMenuFor(null)
                        onEditRule(p)
                      }}
                    >
                      ルールを編集
                    </button>
                    <button type="button" role="menuitem" onClick={() => void refreshSmart(p)}>
                      再評価
                    </button>
                    <button type="button" role="menuitem" onClick={() => void showFb2k(p)}>
                      foobar クエリ
                    </button>
                  </>
                )}
                {EXPORT_PROFILES.map((profile) => (
                  <button key={profile} type="button" role="menuitem" onClick={() => void exportTo(p, profile)}>
                    書き出し: {profile}
                  </button>
                ))}
                <button type="button" role="menuitem" className="danger" onClick={() => void remove(p)}>
                  削除
                </button>
              </div>
            )}
          </li>
        ))}
        {playlists.items.length === 0 && !playlists.error && (
          <li className="muted small" style={{ padding: '2px 6px' }}>
            なし（＋で作成、表の行をここへドラッグで追加）
          </li>
        )}
      </ul>
      {notice && (
        <p className="notice small" onClick={() => onNotice(null)} title="クリックで消す">
          {notice}
        </p>
      )}
      {fb2k && <Fb2kQueryDialog name={fb2k.name} result={fb2k.result} onClose={() => setFb2k(null)} />}
    </section>
  )
}

/** Playlists root 下の m3u8 一覧と取り込み */
function ImportPanel({
  playlists,
  onClose,
  onNotice,
  onOpen,
}: {
  playlists: Playlists
  onClose: () => void
  onNotice: (text: string | null) => void
  onOpen: (id: number) => void
}) {
  const [items, setItems] = useState<ImportCandidate[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState<string | null>(null)
  const [unresolved, setUnresolved] = useState<string[] | null>(null)
  // hook の戻り値は描画ごとに新しいオブジェクトなので、安定な関数だけに依存する
  const { importCandidates } = playlists
  const load = useCallback(() => {
    importCandidates()
      .then((list) => {
        setItems(list)
        setError(null)
      })
      .catch((e: unknown) => setError(e instanceof Error ? e.message : String(e)))
  }, [importCandidates])
  useEffect(() => {
    load()
  }, [load])

  const run = async (path: string, askName: boolean) => {
    let name: string | undefined
    if (askName) {
      name = window.prompt('プレイリスト名', path.replace(/^.*\//, '').replace(/\.m3u8?$/i, ''))?.trim()
      if (!name) return
    }
    setBusy(path)
    try {
      const r = await playlists.importFile(path, name)
      onNotice(
        `「${r.playlist.name}」に ${r.matched} 件を取り込み${r.unresolved.length > 0 ? `、未解決 ${r.unresolved.length} 行` : ''}${
          r.duplicates > 0 ? `、重複 ${r.duplicates} 行` : ''
        }`,
      )
      setUnresolved(r.unresolved.length > 0 ? r.unresolved : null)
      onOpen(r.playlist.id)
    } catch (e) {
      if (e instanceof ApiError && e.code === 'duplicate') {
        // 同名がある: 名前を聞いて取り込み直す
        if (!askName) {
          setBusy(null)
          return run(path, true)
        }
        onNotice('失敗: 同じ名前のプレイリストがある')
      } else {
        onNotice(`失敗: ${e instanceof Error ? e.message : String(e)}`)
      }
    } finally {
      setBusy(null)
    }
  }

  return (
    <div className="import-panel">
      <div className="import-head">
        <span className="muted small">Playlists/ の m3u8</span>
        <span className="spacer" />
        <button type="button" className="ghost" onClick={load} title="一覧を取り直す">
          ↻
        </button>
        <button type="button" className="ghost" onClick={onClose} title="閉じる">
          ×
        </button>
      </div>
      {error && <p className="error small">{error}</p>}
      {items != null && items.length === 0 && <p className="muted small">m3u8 が無い</p>}
      <ul className="flat">
        {(items ?? []).map((c) => (
          <li key={c.path} className="import-row">
            <span className="path" title={`${c.path}（${c.size.toLocaleString('ja-JP')} B）`}>
              {c.path}
            </span>
            <button type="button" className="ghost" disabled={busy != null} onClick={() => void run(c.path, false)}>
              {busy === c.path ? '…' : '取り込む'}
            </button>
          </li>
        ))}
      </ul>
      {unresolved && (
        <details className="small">
          <summary>未解決 {unresolved.length} 行</summary>
          <ul className="flat unresolved">
            {unresolved.slice(0, 200).map((line, i) => (
              <li key={i} title={line}>
                {line}
              </li>
            ))}
            {unresolved.length > 200 && <li className="muted">…他 {unresolved.length - 200} 行</li>}
          </ul>
        </details>
      )}
    </div>
  )
}
