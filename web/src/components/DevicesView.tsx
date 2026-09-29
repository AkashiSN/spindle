// 端末タブ（P5-2、D-95）。左に端末の一覧、右に選んだ端末の 差分 / 選曲 / 設定。
// Android の登録・同期・接続と iPhone の pair は P5-3 / P5-4 で入るので、ここでは案内だけ出す

import { useEffect, useRef, useState } from 'react'
import type { Device, DeviceDiff, DeviceSelection, DeviceVariant, SelectionEstimate } from '../api/types'
import type { Devices } from '../hooks/useDevices'
import { useLocalStorageState } from '../hooks/useLocalStorageState'
import type { Playlists } from '../hooks/usePlaylists'
import { ApiError } from '../api/client'
import { describeEvaluation, deviceMessage, OP_LABELS, sortDiffItems, syncSummary, unsyncedCount } from '../lib/devices'
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
                  <DiffTab device={selected} diff={devices.diff} />
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
  const [form, setForm] = useState(false)
  const [name, setName] = useState('')
  const [variant, setVariant] = useState<DeviceVariant>('aac')
  const [selection, setSelection] = useState<DeviceSelection>('playlists')
  const close = () => {
    setOpen(false)
    setForm(false)
    setName('')
  }

  return (
    <div className="devices-add">
      <button type="button" className="ghost small" onClick={() => (open ? close() : setOpen(true))}>
        端末を追加 {open ? '▾' : '▸'}
      </button>
      {open && !form && (
        <div className="devices-add-menu">
          <button type="button" onClick={() => setForm(true)}>
            iPhone（Mac 経由）
          </button>
          <button type="button" disabled title="USB でつないだ端末を検出して登録します（P5-3 で対応）">
            Android（USB）
          </button>
        </div>
      )}
      {open && form && (
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

// ---------------------------------------------------------------- 差分

function DiffTab({ device, diff }: { device: Device; diff: DeviceDiff | null }) {
  // 評価時刻の説明の基準（描画のたびに時計を読まない）
  const [now] = useState(() => Math.floor(Date.now() / 1000))
  if (diff == null) return <p className="muted">差分を計算中…</p>
  const rows = sortDiffItems(diff.items)
  const shown = rows.slice(0, DIFF_ROW_LIMIT)
  const sync = syncSummary(diff)

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
        <div className="op-row">
          <button type="button" className="primary" disabled title="P5-3 で対応">
            同期（{formatCount(sync.count)} 件・{formatBytes(sync.bytes)}）
          </button>
        </div>
      ) : (
        <p className="small">
          Mac で <code>spindle-agent sync</code> を実行すると反映される。最終報告:{' '}
          {device.last_synced_at != null ? formatDateTime(device.last_synced_at) : 'まだ報告なし'}
        </p>
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
            {estimate.unhashed > 0 && (
              <span className="muted">（容量未確定 {formatCount(estimate.unhashed)} 曲（送る元の準備待ち）は含まない）</span>
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
                <td className="muted">P5-3 で対応</td>
              </tr>
            ) : (
              <tr>
                <th>pair</th>
                <td>
                  <button type="button" disabled title="P5-4 で対応">
                    pair をやり直す
                  </button>
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
      <h2>削除</h2>
      <div className="op-row">
        <button
          type="button"
          className="danger"
          disabled={devices.busy}
          onClick={() => {
            if (window.confirm(`端末「${device.name}」を削除しますか？（端末上のファイルは消しません）`)) {
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
