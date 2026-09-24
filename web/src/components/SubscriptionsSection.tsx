// YouTube 画面の購読の節（SPEC §12.6、D-78、P4-16、D-87）: 追加は番号付きの段（① 再生リスト → ② アルバムとして
// 登録 → ③ 登録して同期）、その下に登録済みの一覧（変更・削除・同期）。
// 行を開くと最終同期の詳細（取れない・揃えられない・別の album・持ち越し・バッチ）

import { useMemo, useState } from 'react'
import type { Subscription } from '../api/types'
import { useCategories } from '../hooks/useCategories'
import { useUrlLookup } from '../hooks/useUrlLookup'
import type { SubscriptionInput, SubscriptionsState } from '../hooks/useSubscriptions'
import { formatDateTime } from '../lib/history'
import { activeSyncJob, listIdFromUrl, subscriptionStatusLabel, syncDetailLines } from '../lib/subscriptions'
import { Step } from './Step'

const EMPTY: SubscriptionInput = {
  url: '',
  albumartist: '',
  album: '',
  category: null,
  align: true,
  enabled: true,
  max_enqueue: 50,
}

export function SubscriptionsSection({ subs, initialUrl = '' }: { subs: SubscriptionsState; initialUrl?: string }) {
  const cats = useCategories(true)
  const [form, setForm] = useState<SubscriptionInput>({ ...EMPTY, url: initialUrl })
  const [open, setOpen] = useState<number | null>(null)
  const [editing, setEditing] = useState<{ id: number; albumartist: string; album: string; category: string | null } | null>(
    null,
  )
  // 登録した購読（③ の結果。「今すぐ同期」の相手）
  const [created, setCreated] = useState<{ sub: Subscription; synced: boolean } | null>(null)
  const { items, jobs, busy } = subs
  const url = form.url.trim()
  const listId = listIdFromUrl(form.url)
  const probeUrls = useMemo(() => (listId != null ? [url] : []), [listId, url])
  const { lookup, probes } = useUrlLookup(probeUrls)
  const probe = listId != null ? probes.get(url) : undefined
  const info = probe?.state === 'ok' ? probe.info : null
  const already = lookup.get(url)?.subscription ?? info?.subscription ?? null
  const urlOk = listId != null && already == null
  const named = form.albumartist.trim() !== '' && form.album.trim() !== ''
  const canAdd = !busy && urlOk && named

  // 列挙で題名が取れたら、空のアルバム名に入れる（人が入れた値は上書きしない）
  const title = info?.title ?? null
  const [filledFrom, setFilledFrom] = useState<string | null>(null)
  if (title != null && filledFrom !== url && form.album.trim() === '') {
    setFilledFrom(url)
    setForm({ ...form, album: title })
  }

  const submit = async (syncNow: boolean) => {
    const sub = await subs.create({ ...form, url, albumartist: form.albumartist.trim(), album: form.album.trim() })
    if (sub == null) return
    setForm(EMPTY)
    setCreated({ sub, synced: false })
    // 同期の投入に失敗したら（409 duplicate・通信失敗）「登録した」だけを出し、「今すぐ同期」を残す
    if (syncNow && (await subs.sync(sub.id))) setCreated({ sub, synced: true })
  }

  return (
    <>
      <Step
        no={1}
        title="再生リスト"
        aside={urlOk ? (info?.title ?? listId) : undefined}
        done={urlOk}
        hint="YouTube の再生リストの URL（list= を含む）。登録すると、同期のたびに Library / Inbox に無い動画だけをダウンロードする"
      >
        <input
          type="url"
          className="subscription-url"
          aria-label="再生リストの URL"
          value={form.url}
          placeholder="https://www.youtube.com/playlist?list=…"
          disabled={busy}
          onChange={(e) => {
            setCreated(null)
            setForm({ ...form, url: e.target.value })
          }}
        />
        {url === '' ? (
          <p className="muted small">再生リストの URL（list= を含む）を貼る</p>
        ) : listId == null ? (
          <p className="error small">YouTube の再生リスト URL（list= 付き）を入れてください（動画 1 本はダウンロードのほうで取る）</p>
        ) : already != null ? (
          <p className="yt-banner warn small">
            この再生リストはもう購読している（{already.albumartist} / {already.album}）。下の一覧から同期する
          </p>
        ) : (
          <table className="kv">
            <tbody>
              <tr>
                <th>再生リスト</th>
                <td>
                  {info?.title ?? '（題名は中身を調べると分かる）'} <code className="muted">{listId}</code>
                </td>
              </tr>
              <tr>
                <th>本数</th>
                <td>
                  {probe == null || probe.state === 'loading'
                    ? '中身を調べている…'
                    : probe.state === 'error'
                      ? `調べられない（${probe.message}）。同期のときに分かる`
                      : `${probe.info.entries} 本（ライブラリに ${probe.info.in_library}・Inbox に ${probe.info.in_inbox} → 番号を揃えるだけ。新規 ${probe.info.new} 本をダウンロード${probe.info.unavailable > 0 ? `。取れない ${probe.info.unavailable} 本は番号を占める` : ''}）`}
                </td>
              </tr>
            </tbody>
          </table>
        )}
      </Step>

      <Step
        no={2}
        title="アルバムとして登録"
        aside="この再生リストを 1 枚のアルバムとして扱う"
        wait={!urlOk}
        done={urlOk && named}
        hint="「揃える」を on にすると、同期のたびに既存の曲のトラック番号とファイル名を再生リストの順に揃える（巻き戻せるバッチ）。非公開・削除の動画も位置を占めるので、その分は番号が飛ぶ"
      >
        <div className="subscription-form">
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
            category（配置先）
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
        </div>
      </Step>

      <Step
        no={3}
        title="登録して同期"
        aside={canAdd ? `「${form.albumartist.trim()} / ${form.album.trim()}」として登録` : created == null ? 'アルバムアーティストとアルバムが要る' : undefined}
        wait={!canAdd && created == null}
        done={created != null}
      >
        {created == null ? (
          <div className="op-row">
            <button type="button" className="primary" disabled={!canAdd} onClick={() => void submit(true)}>
              登録して今すぐ同期
            </button>
            <button type="button" disabled={!canAdd} onClick={() => void submit(false)}>
              登録だけ
            </button>
            <span className="muted small">新規の曲は Inbox に届く（承認は Inbox）</span>
          </div>
        ) : (
          <p className="yt-banner ok small">
            「{created.sub.albumartist} / {created.sub.album}」を登録した。
            {created.synced
              ? '同期を投入した。新規の曲は Inbox に届く（承認は Inbox）'
              : '同期は下の一覧の「同期」か、定期同期（設定していれば）で始まる'}{' '}
            {!created.synced && (
              <button
                type="button"
                disabled={busy || activeSyncJob(jobs, created.sub.id) != null}
                onClick={() => {
                  void subs.sync(created.sub.id).then((ok) => {
                    if (ok) setCreated({ ...created, synced: true })
                  })
                }}
              >
                今すぐ同期
              </button>
            )}{' '}
            <button type="button" className="ghost" onClick={() => setCreated(null)}>
              続けて登録する
            </button>
          </p>
        )}
      </Step>

      <h2>登録済みの購読</h2>
      <p className="muted small">
        同期のたびに Library / Inbox に無い動画だけをダウンロードし、既存の曲の番号とファイル名を再生リストの順に揃える
        （承認は Inbox）。非公開・削除の動画も位置を占めるので、その分は番号が飛ぶ
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
