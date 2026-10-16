// 端末タブ（P5-2、D-95）。左に端末の一覧、右に選んだ端末の 差分 / 選曲 / 設定。
// iPhone は pair コードの発行と、途中の計画の案内・強制破棄だけを持つ（同期は Mac の spindle-agent）

import { useEffect, useRef, useState } from 'react'
import type {
  Device,
  DeviceDiff,
  DeviceSelection,
  DeviceVariant,
  PairCode,
  SelectionEstimate,
  AdoptPreview,
  UnregisteredAdb,
} from '../api/types'
import type { SyncProgress } from '../api/types'
import {
  phaseLabel,
  progressAgeSecs,
  progressRatio,
  progressSummary,
  type ProgressCounts,
} from '../lib/syncProgress'
import type { Devices } from '../hooks/useDevices'
import { useLocalStorageState } from '../hooks/useLocalStorageState'
import { useNow } from '../hooks/useNow'
import type { Playlists } from '../hooks/usePlaylists'
import { ApiError } from '../api/client'
import {
  ADB_DISABLED_MESSAGE,
  AGENT_FORCE_ABANDON_CONFIRM,
  agentPlanText,
  nextPollDelay,
  connectionNote,
  describeEvaluation,
  deviceMessage,
  FORCE_ABANDON_CONFIRM,
  offerForceAbandon,
  OP_LABELS,
  pairCommand,
  pairAdoptCommand,
  queuedSyncText,
  sortDiffItems,
  syncButton,
  syncSummary,
  unsyncedCount,
  volumeLabel,
  volumeAdoptable,
  volumeSelectable,
  adoptPreviewText,
  adoptPreviewRetryDelay,
} from '../lib/devices'
import { formatCount } from '../lib/format'
import { formatDateTime } from '../lib/history'
import { sameIdSet, toggleDraft } from '../lib/devicePicker'
import { Latest } from '../lib/latest'
import { formatBytes } from '../lib/settings'

type Tab = 'diff' | 'selection' | 'settings'
const isTab = (v: unknown): v is Tab => v === 'diff' || v === 'selection' || v === 'settings'

/** 差分表に一度に出す行の上限（全曲の初回同期では数千行になる） */
const DIFF_ROW_LIMIT = 1000

const OPEN_PLAN_MESSAGE = deviceMessage(new ApiError(409, 'open_plan'))

export function DevicesView({
  devices,
  selectedId,
  onSelect,
  playlists,
}: {
  devices: Devices
  selectedId: number | null
  onSelect: (id: number | null) => void
  playlists: Playlists
}) {
  const [tab, setTab] = useLocalStorageState<Tab>('devices.tab', 'diff', isTab)
  const items = devices.items
  const selected = items?.find((d) => d.id === selectedId) ?? null

  // 選んでいた端末が消えた（削除・別タブ）か未選択なら、一覧の先頭を選び直す
  useEffect(() => {
    if (items == null) return
    if (selectedId != null && items.some((d) => d.id === selectedId)) return
    const next = items[0]?.id ?? null
    if (next !== selectedId) onSelect(next)
  }, [items, selectedId, onSelect])

  return (
    <section className="cd devices">
      <div className="table-toolbar">
        <h1>端末</h1>
      </div>
      <p className="muted small">
        ライブラリの曲を端末へ配る。端末ごとに系統（Opus / AAC）と選曲（全曲かプレイリスト）を決めると、ここに端末との差分が出る
      </p>
      {items == null && <p className="muted">読み込み中…</p>}
      {items != null && (
        <div className="devices-body">
          <div className="devices-side">
            {items.length === 0 ? (
              <p className="muted small">端末はまだない。「端末を追加」から登録する</p>
            ) : (
              <ul className="devices-list">
                {items.map((d) => (
                  <li key={d.id} className={d.id === selectedId ? 'selected' : ''}>
                    <button type="button" onClick={() => onSelect(d.id)}>
                      <span className="devices-name">
                        {d.connected != null && (
                          <span
                            className={`dot${d.connected ? ' on' : ''}`}
                            title={d.connected ? '接続中' : '未接続'}
                          />
                        )}
                        {d.name}
                      </span>
                      <span className="muted small">
                        {d.transport === 'agent' ? 'iPhone' : 'Android'} · 未反映 {formatCount(unsyncedCount(d.counts))} /
                        待ち {formatCount(d.counts.waiting)}
                      </span>
                      {connectionNote(d) != null && <span className="muted small">{connectionNote(d)}</span>}
                    </button>
                  </li>
                ))}
              </ul>
            )}
            <AddDevice devices={devices} onCreated={onSelect} />
          </div>
          <div className="devices-detail">
            {devices.error != null && (
              <p className="error">
                {devices.error}{' '}
                <button type="button" className="ghost" onClick={devices.clearError} title="閉じる">
                  ×
                </button>
              </p>
            )}
            {selected != null && (
              <>
                <div className="tabs" role="tablist">
                  {(
                    [
                      ['diff', '差分'],
                      ['selection', '選曲'],
                      ['settings', '設定'],
                    ] as const
                  ).map(([k, label]) => (
                    <button
                      key={k}
                      type="button"
                      role="tab"
                      aria-selected={tab === k}
                      className={tab === k ? 'active' : ''}
                      onClick={() => setTab(k)}
                    >
                      {label}
                    </button>
                  ))}
                </div>
                {tab === 'diff' ? (
                  <DiffTab device={selected} diff={devices.diff} devices={devices} />
                ) : tab === 'selection' ? (
                  <SelectionTab key={selected.id} device={selected} devices={devices} playlists={playlists} />
                ) : (
                  <SettingsTab key={selected.id} device={selected} devices={devices} />
                )}
              </>
            )}
          </div>
        </div>
      )}
    </section>
  )
}

// ---------------------------------------------------------------- 追加

function AddDevice({ devices, onCreated }: { devices: Devices; onCreated: (id: number) => void }) {
  const [open, setOpen] = useState(false)
  const [form, setForm] = useState<'iphone' | 'android' | null>(null)
  const [name, setName] = useState('')
  const [variant, setVariant] = useState<DeviceVariant>('aac')
  const [selection, setSelection] = useState<DeviceSelection>('playlists')
  const close = () => {
    setOpen(false)
    setForm(null)
    setName('')
  }

  return (
    <div className="devices-add">
      <button type="button" className="ghost small" onClick={() => (open ? close() : setOpen(true))}>
        端末を追加 {open ? '▾' : '▸'}
      </button>
      {open && form == null && (
        <div className="devices-add-menu">
          <button type="button" onClick={() => setForm('iphone')}>
            iPhone（Mac 経由）
          </button>
          <button type="button" onClick={() => setForm('android')} title="USB でつないだ端末を検出して登録します">
            Android（USB）
          </button>
        </div>
      )}
      {open && form === 'android' && (
        <AddAndroid
          devices={devices}
          onCancel={close}
          onCreated={(id) => {
            close()
            onCreated(id)
          }}
        />
      )}
      {open && form === 'iphone' && (
        <form
          className="devices-add-form"
          onSubmit={(e) => {
            e.preventDefault()
            void devices.createIphone(name.trim(), variant, selection).then((d) => {
              if (d == null) return
              close()
              onCreated(d.id)
            })
          }}
        >
          <label className="small">
            名前
            <input value={name} onChange={(e) => setName(e.target.value)} placeholder="iPhone" autoFocus />
          </label>
          <label className="small">
            系統
            <select value={variant} onChange={(e) => setVariant(e.target.value === 'opus' ? 'opus' : 'aac')}>
              <option value="aac">AAC</option>
              <option value="opus">Opus</option>
            </select>
          </label>
          <label className="small">
            選曲
            <select
              value={selection}
              onChange={(e) => setSelection(e.target.value === 'all' ? 'all' : 'playlists')}
            >
              <option value="playlists">プレイリスト</option>
              <option value="all">全曲</option>
            </select>
          </label>
          <div className="op-row">
            <button type="submit" className="primary" disabled={devices.busy || name.trim() === ''}>
              追加
            </button>
            <button type="button" className="ghost" onClick={close}>
              キャンセル
            </button>
          </div>
        </form>
      )}
    </div>
  )
}

/** 接続中の未登録 Android を検出して登録する。前の要求が終わってから 3 秒おいて取り直す
 * （開いているフォームごとに要求は高々 1 本。サーバは端末ごとに adb shell を直列に呼ぶので重ねない） */
function AddAndroid({
  devices,
  onCreated,
  onCancel,
}: {
  devices: Devices
  onCreated: (id: number) => void
  onCancel: () => void
}) {
  const [found, setFound] = useState<UnregisteredAdb[] | null>(null)
  const [error, setError] = useState<string | null>(null)
  const { fetchUnregistered } = devices

  useEffect(() => {
    const ac = new AbortController()
    let timer: number | undefined
    const load = async () => {
      let failure: unknown = null
      try {
        const l = await fetchUnregistered(ac.signal)
        if (ac.signal.aborted) return
        setFound(l.items)
        setError(null)
      } catch (e) {
        if (ac.signal.aborted) return
        failure = e
        setError(e instanceof ApiError && e.code === 'adb_disabled' ? ADB_DISABLED_MESSAGE : deviceMessage(e))
      }
      const delay = nextPollDelay(failure)
      if (delay != null) timer = window.setTimeout(() => void load(), delay)
    }
    void load()
    return () => {
      ac.abort()
      window.clearTimeout(timer)
    }
  }, [fetchUnregistered])

  return (
    <div className="devices-add-form">
      {error != null && <p className="error small">{error}</p>}
      {error == null && found == null && <p className="muted small">端末を探しています…</p>}
      {error == null && found != null && found.length === 0 && (
        <p className="muted small">USB でつなぎ、端末で USB デバッグを許可してください</p>
      )}
      {error == null &&
        found?.map((f) =>
          f.state === 'device' ? (
            <AndroidCandidate key={f.serial} found={f} devices={devices} onCreated={onCreated} />
          ) : (
            <p key={f.serial} className="muted small">
              {f.model?.replaceAll('_', ' ') ?? f.serial}: {f.state === 'unauthorized' ? '端末で許可してください' : `接続中（${f.state}）`}
            </p>
          ),
        )}
      <div className="op-row">
        <button type="button" className="ghost" onClick={onCancel}>
          キャンセル
        </button>
      </div>
    </div>
  )
}

function AndroidCandidate({
  found,
  devices,
  onCreated,
}: {
  found: UnregisteredAdb
  devices: Devices
  onCreated: (id: number) => void
}) {
  const [name, setName] = useState((found.model ?? found.serial).replaceAll('_', ' '))
  const [variant, setVariant] = useState<DeviceVariant>('opus')
  const [selection, setSelection] = useState<DeviceSelection>('playlists')
  const [volume, setVolume] = useState<string | null>(null)
  // 選んだ保存先が使えなくなった（空でなくなった・消えた）ら選び直させる
  const chosen = found.volumes.find((v) => v.volume === volume && volumeSelectable(v)) ?? null
  const adopting = chosen != null && volumeAdoptable(chosen)
  // 取った結果は取った保存先の印（volume）と組で持つ。選び直した直後に前の結果を出さない
  const [fetched, setFetched] = useState<
    { volume: string; preview: AdoptPreview | null; error: string | null } | null
  >(null)
  const { fetchAdoptPreview } = devices
  const chosenVolume = chosen?.volume ?? null
  const current = fetched != null && fetched.volume === chosenVolume ? fetched : null
  const preview = current?.preview ?? null
  const previewError = current?.error ?? null

  // スキャン中か失敗なら間を置いて取り直す（同じ保存先を選んでいる間だけ。取れたら止める）
  const [retry, setRetry] = useState(0)
  const retryDelay = adopting ? adoptPreviewRetryDelay(current) : null
  useEffect(() => {
    if (retryDelay == null) return
    const t = setTimeout(() => setRetry((n) => n + 1), retryDelay)
    return () => clearTimeout(t)
  }, [retryDelay, current])

  // 引き継ぐ保存先を選んだら、引き継ぐ件数を取る
  useEffect(() => {
    if (!adopting || chosenVolume == null) return
    const ac = new AbortController()
    fetchAdoptPreview(found.serial, chosenVolume, ac.signal)
      .then((p) => {
        if (!ac.signal.aborted) setFetched({ volume: chosenVolume, preview: p, error: null })
      })
      .catch((e) => {
        if (!ac.signal.aborted) setFetched({ volume: chosenVolume, preview: null, error: deviceMessage(e) })
      })
    return () => ac.abort()
  }, [adopting, chosenVolume, found.serial, fetchAdoptPreview, retry])

  return (
    <form
      className="devices-add-form devices-android"
      onSubmit={(e) => {
        e.preventDefault()
        if (chosen == null) return
        void devices
          .registerAndroid({ name: name.trim(), variant, selection, serial: found.serial, volume: chosen.volume, adopt: adopting })
          .then((d) => {
            if (d != null) onCreated(d.id)
          })
      }}
    >
      <strong className="small">{found.model?.replaceAll('_', ' ') ?? found.serial}</strong>
      {found.error != null && <span className="error small">{found.error}</span>}
      <p className="muted small">
        Poweramp は設定したフォルダだけをスキャンします。保存先が Poweramp の音楽フォルダの下にあることを確かめてください
      </p>
      <label className="small">
        名前
        <input value={name} onChange={(e) => setName(e.target.value)} />
      </label>
      <label className="small">
        系統
        <select value={variant} onChange={(e) => setVariant(e.target.value === 'aac' ? 'aac' : 'opus')}>
          <option value="opus">Opus</option>
          <option value="aac">AAC</option>
        </select>
      </label>
      <label className="small">
        選曲
        <select value={selection} onChange={(e) => setSelection(e.target.value === 'all' ? 'all' : 'playlists')}>
          <option value="playlists">プレイリスト</option>
          <option value="all">全曲</option>
        </select>
      </label>
      <fieldset className="small">
        <legend>保存先</legend>
        {found.volumes.length === 0 && <span className="muted">保存先を検出できません</span>}
        {found.volumes.map((v) => (
          <label key={v.volume} className="devices-volume" title={v.path}>
            <input
              type="radio"
              name={`vol-${found.serial}`}
              checked={chosen?.volume === v.volume}
              disabled={!volumeSelectable(v)}
              onChange={() => setVolume(v.volume)}
            />
            {volumeLabel(v)} <span className="muted">空き {formatBytes(v.free)}</span>
            {volumeAdoptable(v) && <span className="muted"> · 前の登録の保存先（引き継げる）</span>}
            {!volumeSelectable(v) && <span className="muted"> · 空でない</span>}
          </label>
        ))}
      </fieldset>
      {adopting && previewError != null && <p className="error small">{previewError}</p>}
      {adopting && preview != null && !preview.library_busy && <p className="small">{adoptPreviewText(preview)}</p>}
      {adopting && preview?.library_busy === true && (
        <p className="devices-warn small">ライブラリのスキャン中です。終わってから引き継いでください</p>
      )}
      {adopting && selection === 'playlists' && (
        <p className="muted small">引き継いだプレイリストには印を付け直します</p>
      )}
      <div className="op-row">
        <button type="submit" className="primary" disabled={
            devices.busy ||
            name.trim() === '' ||
            chosen == null ||
            (adopting && (preview == null || preview.library_busy))
          }
        >
          {adopting ? '引き継いで登録' : '登録'}
        </button>
      </div>
    </form>
  )
}

// ---------------------------------------------------------------- 差分

function DiffTab({ device, diff, devices }: { device: Device; diff: DeviceDiff | null; devices: Devices }) {
  // 評価時刻の説明の基準（描画のたびに時計を読まない）
  const [now] = useState(() => Math.floor(Date.now() / 1000))
  // 進捗の経過秒と「同期中か」の判定は毎秒読み直す（進捗がある間だけ）
  const tick = useNow(1000, device.sync_progress != null)
  if (diff == null) return <p className="muted">差分を計算中…</p>
  const rows = sortDiffItems(diff.items)
  const shown = rows.slice(0, DIFF_ROW_LIMIT)
  const sync = syncSummary(diff)
  const button = syncButton(device, diff)
  // Android の同期の進捗（job イベントで届く）。開き直した直後は次のイベントまで無いので「同期中…」
  // 表は running の間だけ項目を持つので、一覧の state が queued のままでも出してよい（取り直しを待たない）
  const adbProgress = device.sync_job != null ? devices.jobProgress.get(device.sync_job.id) : undefined

  return (
    <div className="devices-diff">
      <p className="small">
        送る量 {formatBytes(diff.estimate.transfer_bytes)} · 一時的に増える量 {formatBytes(diff.estimate.peak_bytes)}
        {diff.estimate.free != null && <> · 端末の空き {formatBytes(diff.estimate.free)}</>}
      </p>
      {diff.pending_reevaluation && (
        <p className="devices-warn small">
          スマートプレイリストの評価待ちがある。評価が終わると差分が変わることがある
        </p>
      )}
      {diff.evaluations.length > 0 && (
        <ul className="devices-evaluations muted small">
          {diff.evaluations.map((e) => (
            <li key={e.playlist_id}>{describeEvaluation(e, now)}</li>
          ))}
        </ul>
      )}
      {device.transport === 'adb' ? (
        <>
          {connectionNote(device) != null && <p className="devices-warn small">{connectionNote(device)}</p>}
          {device.plan_open && device.sync_job?.state !== 'running' && (
            <div className="devices-warn small">
              <p>前回の同期が途中です</p>
              <div className="op-row">
                {device.sync_job == null && (
                  <button type="button" disabled={devices.busy} onClick={() => void devices.resume(device.id)}>
                    続きを実行
                  </button>
                )}
                <button
                  type="button"
                  className="danger"
                  disabled={devices.busy}
                  onClick={() => {
                    if (window.confirm('途中の計画を破棄します。端末に送り終えた分はそのまま残ります。よろしいですか？')) {
                      void devices.abandon(device.id)
                    }
                  }}
                >
                  破棄
                </button>
                {offerForceAbandon(device, devices.errorCode) && (
                  <button
                    type="button"
                    className="danger"
                    disabled={devices.busy}
                    title="端末につながずに計画を閉じます"
                    onClick={() => {
                      if (window.confirm(FORCE_ABANDON_CONFIRM)) void devices.forceAbandon(device.id)
                    }}
                  >
                    強制破棄
                  </button>
                )}
              </div>
            </div>
          )}
          {adbProgress != null ? (
            <SyncProgressLine p={adbProgress} age={null} />
          ) : (
            device.sync_job != null && (
              <p className="small">{device.sync_job.state === 'running' ? '同期中…' : queuedSyncText(device)}</p>
            )
          )}
          <div className="op-row">
            <button
              type="button"
              className="primary"
              disabled={devices.busy || !button.enabled}
              title={button.title ?? undefined}
              onClick={() => void devices.sync(device.id, diff.plan_token)}
            >
              同期（{formatCount(sync.count)} 件・{formatBytes(sync.bytes)}）
            </button>
          </div>
        </>
      ) : (
        <>
          {agentPlanText(device, tick) != null && (
            <div className="devices-warn small">
              <p>{agentPlanText(device, tick)}</p>
              <div className="op-row">
                <button
                  type="button"
                  className="danger"
                  disabled={devices.busy}
                  title="Mac につながずに計画を閉じます"
                  onClick={() => {
                    if (window.confirm(AGENT_FORCE_ABANDON_CONFIRM)) void devices.forceAbandon(device.id)
                  }}
                >
                  強制破棄
                </button>
              </div>
            </div>
          )}
          {/* 進捗はサーバが open な計画の分だけ返し・流す。確定はイベントを流さないので plan_open は見ない */}
          {device.sync_progress && (
            <SyncProgressLine p={device.sync_progress} age={progressAgeSecs(device.sync_progress, tick)} />
          )}
          <p className="small">
            Mac で <code>spindle-agent sync</code> を実行すると反映される。最終報告:{' '}
            {device.last_synced_at != null ? formatDateTime(device.last_synced_at) : 'まだ報告なし'}
          </p>
        </>
      )}
      {rows.length === 0 ? (
        <p className="muted">差分はない（反映済み {formatCount(diff.counts.synced)} 曲）</p>
      ) : (
        <table className="gc-table devices-diff-table">
          <thead>
            <tr>
              <th>操作</th>
              <th>曲</th>
              <th>理由</th>
            </tr>
          </thead>
          <tbody>
            {shown.map((i) => (
              <tr key={`${i.op}:${i.track_id}`} className={`op-${i.op}`}>
                <td className="devices-op">{OP_LABELS[i.op]}</td>
                <td title={i.dest_path ?? undefined}>
                  {i.title != null ? (
                    <>
                      {i.title}
                      {i.artist != null && <span className="muted"> — {i.artist}</span>}
                    </>
                  ) : (
                    (i.dest_path ?? `#${i.track_id}`)
                  )}
                </td>
                <td className="small">
                  {i.reason}
                  {(i.op === 'waiting' || i.op === 'error') && i.has_copy && (
                    <span className="muted">{i.reason != null ? ' · ' : ''}保留: 既存の写しは残す</span>
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
      {rows.length > shown.length && (
        <p className="muted small">ほか {formatCount(rows.length - shown.length)} 件（先頭 {formatCount(DIFF_ROW_LIMIT)} 件だけ表示）</p>
      )}
      {diff.playlists.length > 0 && (
        <>
          <h2>プレイリスト</h2>
          <table className="gc-table devices-diff-table small">
            <tbody>
              {diff.playlists.map((p) => (
                <tr key={`${p.op}:${p.playlist_id}`} className={`op-${p.op}`}>
                  <td className="devices-op">{OP_LABELS[p.op]}</td>
                  <td title={p.dest_path ?? undefined}>{p.name ?? p.dest_path ?? `#${p.playlist_id}`}</td>
                  <td>{p.reason}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </>
      )}
    </div>
  )
}

// ---------------------------------------------------------------- 選曲

function SelectionTab({ device, devices, playlists }: { device: Device; devices: Devices; playlists: Playlists }) {
  const [selection, setSelection] = useState<DeviceSelection>(device.selection)
  // 印の下書き。null は未編集で、保存済みの値（サイドバーでの付け外しを含む）をそのまま出す。
  // 開いた時点の値を写して持つと、開いている間のサイドバーでの変更を次の保存で黙って戻してしまう
  const [draft, setDraft] = useState<number[] | null>(null)
  const ids = draft ?? device.playlist_ids
  const [estimate, setEstimate] = useState<SelectionEstimate | null>(null)
  const gen = useRef(new Latest())
  const { estimate: fetchEstimate } = devices

  // 選び方を変えるたびに見積もりを取り直す（最新の要求の応答だけ採る）
  const idsKey = ids.join(',')
  useEffect(() => {
    const g = gen.current
    const id = g.next()
    const list = idsKey === '' ? [] : idsKey.split(',').map(Number)
    fetchEstimate(device.id, selection, list)
      .then((e) => {
        if (g.isCurrent(id)) setEstimate(e)
      })
      .catch(() => {
        if (g.isCurrent(id)) setEstimate(null)
      })
    return () => g.invalidate()
  }, [fetchEstimate, device.id, selection, idsKey])

  const selectionChanged = selection !== device.selection
  const idsChanged = !sameIdSet(ids, device.playlist_ids)
  const toggle = (id: number, on: boolean) => setDraft((cur) => toggleDraft(cur, device.playlist_ids, id, on))
  const save = async () => {
    // 印を先に保存する（プレイリスト選曲へ切り替えるとき、空の選曲の差分を一瞬でも作らない）
    if (idsChanged) {
      if (!(await devices.setPlaylists(device.id, ids))) return
      // 保存したら未編集に戻す（以後は保存済みの値に追随する）
      setDraft(null)
    }
    if (selectionChanged) await devices.update(device.id, { selection })
  }

  return (
    <div className="devices-selection">
      <div className="seg" role="tablist" aria-label="選曲">
        {(
          [
            ['all', '全曲'],
            ['playlists', 'プレイリスト'],
          ] as const
        ).map(([k, label]) => (
          <button
            key={k}
            type="button"
            role="tab"
            aria-selected={selection === k}
            className={selection === k ? 'on' : ''}
            onClick={() => setSelection(k)}
          >
            {label}
          </button>
        ))}
      </div>
      {selection === 'playlists' && (
        <ul className="devices-playlists">
          {playlists.items.length === 0 && <li className="muted small">プレイリストがない</li>}
          {playlists.items.map((p) => (
            <li key={p.id}>
              <label>
                <input type="checkbox" checked={ids.includes(p.id)} onChange={(e) => toggle(p.id, e.target.checked)} />
                {p.kind === 'smart' && <span title="スマートプレイリスト">⚙ </span>}
                {p.name} <span className="muted small">{formatCount(p.track_count)} 曲</span>
              </label>
            </li>
          ))}
        </ul>
      )}
      <p className="small">
        {estimate == null ? (
          <span className="muted">見積もり中…</span>
        ) : (
          <>
            合計 {formatCount(estimate.tracks)} 曲・約 {formatBytes(estimate.bytes)}
            {estimate.estimated > 0 && (
              <span className="muted">（うち {formatCount(estimate.estimated)} 曲は長さとビットレートからの推定）</span>
            )}
            {estimate.unhashed > 0 && (
              <span className="muted">（容量の分からない {formatCount(estimate.unhashed)} 曲は含まない）</span>
            )}
          </>
        )}
      </p>
      {device.open_plan && <p className="muted small">{OPEN_PLAN_MESSAGE}</p>}
      <div className="op-row">
        <button
          type="button"
          className="primary"
          disabled={devices.busy || device.open_plan || (!selectionChanged && !idsChanged)}
          onClick={() => void save()}
        >
          保存
        </button>
      </div>
    </div>
  )
}

// ---------------------------------------------------------------- 設定

function SettingsTab({ device, devices }: { device: Device; devices: Devices }) {
  const [name, setName] = useState(device.name)
  const [variant, setVariant] = useState<DeviceVariant>(device.variant)
  const nameChanged = name.trim() !== '' && name.trim() !== device.name
  const variantChanged = variant !== device.variant

  return (
    <div className="devices-settings">
      <form
        onSubmit={(e) => {
          e.preventDefault()
          void devices.update(device.id, {
            ...(nameChanged ? { name: name.trim() } : {}),
            // 同期が途中のときは系統を送らない（名前だけの保存を 409 で弾かせない）
            ...(variantChanged && !device.open_plan ? { variant } : {}),
          })
        }}
      >
        <table className="kv">
          <tbody>
            <tr>
              <th>名前</th>
              <td>
                <input value={name} onChange={(e) => setName(e.target.value)} />
              </td>
            </tr>
            <tr>
              <th>系統</th>
              <td>
                <select
                  value={variant}
                  disabled={device.open_plan}
                  onChange={(e) => setVariant(e.target.value === 'opus' ? 'opus' : 'aac')}
                >
                  <option value="aac">AAC</option>
                  <option value="opus">Opus</option>
                </select>
                {device.open_plan && <span className="muted small"> {OPEN_PLAN_MESSAGE}</span>}
              </td>
            </tr>
            {device.transport === 'adb' ? (
              <tr>
                <th>保存先</th>
                <td>
                  {device.adb_volume != null
                    ? volumeLabel({ volume: device.adb_volume, path: '', free: 0, state: 'empty' })
                    : '未設定'}
                  {device.adb_root != null && <span className="muted"> · {device.adb_root}</span>}
                  <p className="muted small">
                    保存先を変えるには、この端末を削除してから登録し直します（新しい保存先は空であること）
                  </p>
                </td>
              </tr>
            ) : (
              <tr>
                <th>pair</th>
                <td>
                  <PairCodeCell device={device} devices={devices} />
                </td>
              </tr>
            )}
          </tbody>
        </table>
        <div className="op-row">
          <button
            type="submit"
            className="primary"
            disabled={devices.busy || (!nameChanged && !(variantChanged && !device.open_plan))}
          >
            保存
          </button>
        </div>
      </form>
      {device.transport === 'adb' && (
        <>
          <h2>検証</h2>
          <div className="op-row">
            <button
              type="button"
              disabled={devices.busy || device.sync_job != null}
              title="端末上の全曲の sha256 を確かめます。食い違った曲は次の同期で送り直します"
              onClick={() => void devices.verify(device.id)}
            >
              内容を検証
            </button>
          </div>
        </>
      )}
      <h2>削除</h2>
      <div className="op-row">
        <button
          type="button"
          className="danger"
          disabled={devices.busy || device.sync_job?.state === 'running'}
          title={device.sync_job?.state === 'running' ? '同期中は削除できません' : undefined}
          onClick={() => {
            const plan = device.plan_open ? '途中の計画も破棄します。' : ''
            if (window.confirm(`端末「${device.name}」を削除しますか？（${plan}端末上のファイルは消しません）`)) {
              void devices.remove(device.id)
            }
          }}
        >
          端末を削除
        </button>
        <span className="muted small">端末上のファイルは消しません</span>
      </div>
    </div>
  )
}

/** iPhone のエージェントの pair コードを発行して出す。コードはこの画面の state にだけ持ち、再読み込みで消える */
function PairCodeCell({ device, devices }: { device: Device; devices: Devices }) {
  const [issued, setIssued] = useState<PairCode | null>(null)
  // コピーしたコマンド（通常の pair か引き継ぎか）
  const [copied, setCopied] = useState<string | null>(null)
  const command = issued != null ? pairCommand(window.location.origin, issued.code) : null
  const adoptCommand = issued != null ? pairAdoptCommand(window.location.origin, issued.code) : null
  const issue = async () => {
    if (!window.confirm('発行すると今の Mac のトークンは使えなくなります。よろしいですか？')) return
    setCopied(null)
    const p = await devices.pairCode(device.id)
    if (p != null) setIssued(p)
  }
  const copy = async (text: string) => {
    try {
      await navigator.clipboard.writeText(text)
      setCopied(text)
    } catch {
      setCopied(null)
    }
  }
  return (
    <div className="devices-pair">
      <button
        type="button"
        disabled={devices.busy || device.open_plan}
        title={device.open_plan ? '同期が途中なので発行できない' : undefined}
        onClick={() => void issue()}
      >
        pair コードを発行
      </button>
      {issued != null && command != null && adoptCommand != null && (
        <div className="small">
          <p>
            コード <code>{issued.code}</code>（期限 {formatDateTime(issued.expires_at)}、1 回限り）。Mac で次を実行する
          </p>
          <p>
            <code>{command}</code>{' '}
            <button type="button" onClick={() => void copy(command)}>
              {copied === command ? 'コピーした' : 'コピー'}
            </button>
          </p>
          <p className="muted">この Mac に前の登録の保存先が残っていれば、代わりに次を実行すると引き継げます</p>
          <p>
            <code>{adoptCommand}</code>{' '}
            <button type="button" onClick={() => void copy(adoptCommand)}>
              {copied === adoptCommand ? 'コピーした' : 'コピー'}
            </button>
          </p>
        </div>
      )}
    </div>
  )
}

/**
 * 同期の進捗。`age` は最後の進捗からの経過秒（Mac は受信時刻から 1 秒ごとに数え直す。サーバの age は
 * 受信時の値）。null なら「応答なし」を出さない（Android。ジョブの生死はワーカーが管理する）
 */
function SyncProgressLine({
  p,
  age,
}: {
  p: ProgressCounts & Pick<SyncProgress, 'phase' | 'current'>
  age: number | null
}) {
  const ratio = progressRatio(p)
  // age が無い（Android）なら経過 0 で「応答なし」を出さない
  const summary = age == null ? progressSummary(p, 0, { stale: false }) : progressSummary(p, age, { stale: true })
  return (
    <div className="sync-progress small">
      <div className="sync-progress-head">
        <span>{phaseLabel(p.phase)}</span>
        {p.current != null && <span className="muted sync-progress-current">{p.current}</span>}
      </div>
      <progress className="sync-progress-bar" value={ratio} max={1} aria-label="同期の進捗" />
      <div className="muted">{summary}</div>
    </div>
  )
}
