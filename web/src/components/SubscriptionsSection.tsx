// YouTube 画面の購読の節（SPEC §12.6、D-78、P4-16）: 再生リストの購読の一覧・追加・変更・削除・同期。
// 行を開くと最終同期の詳細（取れない・揃えられない・別の album・持ち越し・バッチ）

import { useState } from 'react'
import type { Subscription } from '../api/types'
import { useCategories } from '../hooks/useCategories'
import type { SubscriptionInput, SubscriptionsState } from '../hooks/useSubscriptions'
import { formatDateTime } from '../lib/history'
import { activeSyncJob, listIdFromUrl, subscriptionStatusLabel, syncDetailLines } from '../lib/subscriptions'

const EMPTY: SubscriptionInput = {
  url: '',
  albumartist: '',
  album: '',
  category: null,
  align: true,
  enabled: true,
  max_enqueue: 50,
}

export function SubscriptionsSection({ subs }: { subs: SubscriptionsState }) {
  const cats = useCategories(true)
  const [form, setForm] = useState<SubscriptionInput>(EMPTY)
  const [open, setOpen] = useState<number | null>(null)
  const [editing, setEditing] = useState<{ id: number; albumartist: string; album: string; category: string | null } | null>(
    null,
  )
  const { items, jobs, busy } = subs
  const listId = listIdFromUrl(form.url)
  const canAdd = !busy && listId != null && form.albumartist.trim() !== '' && form.album.trim() !== ''

  const submit = async () => {
    if (await subs.create({ ...form, albumartist: form.albumartist.trim(), album: form.album.trim() })) {
      setForm(EMPTY)
    }
  }

  return (
    <>
      <h2>購読</h2>
      <p className="muted small">
        再生リストを登録しておくと、同期のたびに Library / Inbox に無い動画だけをダウンロードし、既存の曲の番号と
        ファイル名を再生リストの順に揃える（承認は Inbox）。非公開・削除の動画も位置を占めるので、その分は番号が飛ぶ
      </p>
      {items == null ? (
        <p className="muted">読み込み中…</p>
      ) : items.length === 0 ? (
        <p className="muted">購読はまだ無い</p>
      ) : (
        <table className="history-table subscriptions">
          <thead>
            <tr>
              <th>アルバムアーティスト / アルバム</th>
              <th>再生リスト</th>
              <th>有効</th>
              <th>揃える</th>
              <th>最終同期</th>
              <th>結果</th>
              <th />
            </tr>
          </thead>
          <tbody>
            {items.map((s) => {
              const job = activeSyncJob(jobs, s.id)
              const lines = s.last_result != null ? syncDetailLines(s.last_result) : []
              const isOpen = open === s.id
              return (
                <SubscriptionRow
                  key={s.id}
                  s={s}
                  status={subscriptionStatusLabel(s, job)}
                  failed={s.last_result?.state === 'failed'}
                  lines={lines}
                  isOpen={isOpen}
                  busy={busy}
                  syncing={job != null}
                  editing={editing?.id === s.id ? editing : null}
                  categories={cats.items.map((c) => c.name)}
                  onToggle={() => setOpen(isOpen ? null : s.id)}
                  onSync={() => void subs.sync(s.id)}
                  onEnabled={(v) => void subs.update(s.id, { enabled: v })}
                  onAlign={(v) => void subs.update(s.id, { align: v })}
                  onEdit={() => setEditing({ id: s.id, albumartist: s.albumartist, album: s.album, category: s.category })}
                  onEditChange={(e) => setEditing({ id: s.id, ...e })}
                  onEditSave={async () => {
                    if (editing == null) return
                    const ok = await subs.update(s.id, {
                      albumartist: editing.albumartist.trim(),
                      album: editing.album.trim(),
                      category: editing.category,
                    })
                    if (ok) setEditing(null)
                  }}
                  onEditCancel={() => setEditing(null)}
                  onRemove={() => {
                    if (window.confirm(`購読を削除しますか？（Library のファイルは消えない）\n${s.albumartist} / ${s.album}`)) {
                      void subs.remove(s.id)
                    }
                  }}
                />
              )
            })}
          </tbody>
        </table>
      )}

      <h3>購読を追加</h3>
      <div className="subscription-form">
        <label>
          再生リストの URL
          <input
            type="url"
            value={form.url}
            placeholder="https://www.youtube.com/playlist?list=…"
            disabled={busy}
            onChange={(e) => setForm({ ...form, url: e.target.value })}
          />
        </label>
        <label>
          アルバムアーティスト
          <input
            type="text"
            value={form.albumartist}
            disabled={busy}
            onChange={(e) => setForm({ ...form, albumartist: e.target.value })}
          />
        </label>
        <label>
          アルバム
          <input type="text" value={form.album} disabled={busy} onChange={(e) => setForm({ ...form, album: e.target.value })} />
        </label>
        <label>
          category
          <select
            value={form.category ?? ''}
            disabled={busy}
            onChange={(e) => setForm({ ...form, category: e.target.value === '' ? null : e.target.value })}
          >
            <option value="">（未分類 = _Unsorted）</option>
            {cats.items.map((c) => (
              <option key={c.id} value={c.name}>
                {c.name}
              </option>
            ))}
          </select>
        </label>
        <label className="inline">
          <input type="checkbox" checked={form.align} disabled={busy} onChange={(e) => setForm({ ...form, align: e.target.checked })} />
          番号とファイル名を再生リストの順に揃える
        </label>
        <label className="inline">
          1 回の同期で投入する上限
          <input
            type="number"
            min={1}
            max={1000}
            value={form.max_enqueue}
            disabled={busy}
            onChange={(e) => setForm({ ...form, max_enqueue: Math.max(1, Number(e.target.value) || 1) })}
          />
        </label>
        <div className="op-row">
          <button type="button" className="primary" disabled={!canAdd} onClick={() => void submit()}>
            追加
          </button>
          {form.url.trim() !== '' && listId == null && (
            <span className="error small">YouTube の再生リスト URL（list= 付き）を入れてください</span>
          )}
        </div>
      </div>
    </>
  )
}

function SubscriptionRow({
  s,
  status,
  failed,
  lines,
  isOpen,
  busy,
  syncing,
  editing,
  categories,
  onToggle,
  onSync,
  onEnabled,
  onAlign,
  onEdit,
  onEditChange,
  onEditSave,
  onEditCancel,
  onRemove,
}: {
  s: Subscription
  status: string
  failed: boolean
  lines: string[]
  isOpen: boolean
  busy: boolean
  syncing: boolean
  editing: { albumartist: string; album: string; category: string | null } | null
  categories: string[]
  onToggle: () => void
  onSync: () => void
  onEnabled: (v: boolean) => void
  onAlign: (v: boolean) => void
  onEdit: () => void
  onEditChange: (e: { albumartist: string; album: string; category: string | null }) => void
  onEditSave: () => void
  onEditCancel: () => void
  onRemove: () => void
}) {
  return (
    <>
      <tr className={failed ? 'failed' : undefined}>
        <td>
          {editing != null ? (
            <span className="subscription-edit">
              <input
                type="text"
                aria-label="アルバムアーティスト"
                value={editing.albumartist}
                onChange={(e) => onEditChange({ ...editing, albumartist: e.target.value })}
              />
              <input
                type="text"
                aria-label="アルバム"
                value={editing.album}
                onChange={(e) => onEditChange({ ...editing, album: e.target.value })}
              />
              <select
                aria-label="category"
                value={editing.category ?? ''}
                onChange={(e) => onEditChange({ ...editing, category: e.target.value === '' ? null : e.target.value })}
              >
                <option value="">（未分類）</option>
                {categories.map((c) => (
                  <option key={c} value={c}>
                    {c}
                  </option>
                ))}
              </select>
            </span>
          ) : (
            <>
              <strong>{s.albumartist}</strong> / {s.album}
              {s.category != null && <span className="muted small"> [{s.category}]</span>}
              {s.album_id != null && <span className="muted small"> album #{s.album_id}</span>}
            </>
          )}
        </td>
        <td className="path">
          <a href={s.url} target="_blank" rel="noreferrer noopener">
            {s.list_id}
          </a>
        </td>
        <td>
          <input type="checkbox" aria-label="有効" checked={s.enabled} disabled={busy || syncing} onChange={(e) => onEnabled(e.target.checked)} />
        </td>
        <td>
          <input type="checkbox" aria-label="揃える" checked={s.align} disabled={busy || syncing} onChange={(e) => onAlign(e.target.checked)} />
        </td>
        <td className="nowrap muted">{s.last_synced_at != null ? formatDateTime(s.last_synced_at) : '—'}</td>
        <td>
          {status}
          {lines.length > 0 && (
            <>
              {' '}
              <button type="button" className="ghost" onClick={onToggle}>
                {isOpen ? '閉じる' : '詳細'}
              </button>
            </>
          )}
        </td>
        <td className="nowrap">
          {editing != null ? (
            <>
              <button type="button" className="ghost" disabled={busy} onClick={onEditSave}>
                保存
              </button>
              <button type="button" className="ghost" onClick={onEditCancel}>
                取消
              </button>
            </>
          ) : (
            <>
              <button type="button" className="ghost" disabled={busy || syncing} onClick={onSync}>
                同期
              </button>
              <button type="button" className="ghost" disabled={busy || syncing} onClick={onEdit} title={syncing ? '同期中は変更できない' : undefined}>
                編集
              </button>
              <button type="button" className="ghost" disabled={busy || syncing} onClick={onRemove} title={syncing ? '同期中は削除できない' : undefined}>
                削除
              </button>
            </>
          )}
        </td>
      </tr>
      {isOpen && lines.length > 0 && (
        <tr className="subscription-detail">
          <td colSpan={7}>
            <ul>
              {lines.map((l, i) => (
                <li key={i}>{l}</li>
              ))}
            </ul>
          </td>
        </tr>
      )}
    </>
  )
}
