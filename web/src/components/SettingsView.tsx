// 設定画面（SPEC §12.6、D-58）: config.toml の閲覧、再スキャン / deep scan、GC の preview → 実行、
// 退避ファイル（archived_files）の一覧。復元は対応バッチの履歴から巻き戻す（バッチ #n で履歴へ）

import { useCategories } from '../hooks/useCategories'
import type { SettingsState } from '../hooks/useSettings'
import type { ThemeState } from '../hooks/useTheme'
import { formatCount } from '../lib/format'
import { formatDateTime } from '../lib/history'
import { deleteConfirmText } from '../lib/categories'
import { archiveReasonLabel, archiveStateLabel, formatBytes, gcPreviewRows } from '../lib/settings'
import { THEME_PREFS } from '../lib/theme'

export function SettingsView({
  settings,
  theme,
  onOpenBatch,
}: {
  settings: SettingsState
  theme: ThemeState
  onOpenBatch: (batchId: number) => void
}) {
  const { config, archive, gcPreview, busy, notice, error } = settings
  const gcRows = gcPreview ? gcPreviewRows(gcPreview) : null
  const gcTotal = gcRows ? gcRows.reduce((a, r) => a + r.count, 0) : 0
  const cats = useCategories(true)

  return (
    <section className="settings">
      <div className="table-toolbar">
        <h1>設定</h1>
        <span className="spacer" />
        {notice != null && (
          <span className="small">
            {notice}{' '}
            <button type="button" className="ghost" onClick={settings.clearNotice} title="閉じる">
              ×
            </button>
          </span>
        )}
        <button type="button" className="ghost" onClick={settings.refresh}>
          更新
        </button>
      </div>
      {error != null && <p className="error">{error}</p>}

      <h2>表示</h2>
      <fieldset className="theme-pref">
        <legend className="small">配色</legend>
        {THEME_PREFS.map(([pref, label]) => (
          <label key={pref} className="small">
            <input type="radio" name="theme-pref" checked={theme.pref === pref} onChange={() => theme.setPref(pref)} />{' '}
            {label}
          </label>
        ))}
        <span className="muted small">このブラウザにだけ保存する（localStorage）。OS に従う は prefers-color-scheme の変化に追随する</span>
      </fieldset>

      <h2>ライブラリ</h2>
      <div className="op-row">
        <button type="button" disabled={busy != null} onClick={() => void settings.startScan('incremental')}>
          {busy === 'scan:incremental' ? '投入中…' : '再スキャン'}
        </button>
        <button type="button" disabled={busy != null} onClick={() => void settings.startScan('deep')}>
          {busy === 'scan:deep' ? '投入中…' : 'deep scan'}
        </button>
        <span className="muted small">
          再スキャンは stat の差分だけ。deep scan は全件のタグと音声ハッシュを読み直す（時間がかかる）
        </span>
      </div>

      <h2>GC</h2>
      <div className="op-row">
        <button type="button" disabled={busy != null} onClick={() => void settings.loadGcPreview()}>
          {busy === 'gc:preview' ? '確認中…' : '消すものを確認（dry-run）'}
        </button>
        {gcRows && (
          <button type="button" className="primary" disabled={busy != null || gcTotal === 0} onClick={() => void settings.startGc()}>
            {busy === 'gc:start' ? '投入中…' : `GC を実行（${formatCount(gcTotal)} 件）`}
          </button>
        )}
        <span className="muted small">物理削除は GC だけが行う。定期実行は 1 日 1 回、保持期間は [gc].retention_days</span>
      </div>
      {gcRows && (
        <table className="gc-table">
          <thead>
            <tr>
              <th>区分</th>
              <th className="num">件数</th>
              <th className="num">サイズ</th>
              <th>先頭</th>
            </tr>
          </thead>
          <tbody>
            {gcRows.map((r) => (
              <tr key={r.key} className={r.count === 0 ? 'muted' : ''}>
                <td>{r.label}</td>
                <td className="num">{formatCount(r.count)}</td>
                <td className="num">{r.bytes == null ? '' : formatBytes(r.bytes)}</td>
                <td className="sample">
                  {r.sample.slice(0, 5).join('、')}
                  {r.count > 5 && <span className="muted"> …</span>}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <h2>category</h2>
      <p className="muted small">
        配置先の最上位のフォルダ。Library 直下のフォルダ名からスキャンで自動で作られる（追加だけ）。
        使われていない語彙（どのアルバム・購読・Inbox の下書きにも使われていない）は削除できる
      </p>
      {cats.error != null && <p className="error">{cats.error}</p>}
      {cats.items.length === 0 ? (
        <p className="muted">category はまだない</p>
      ) : (
        <ul className="category-list">
          {cats.items.map((c) => (
            <li key={c.id}>
              <span>{c.name}</span>{' '}
              <button
                type="button"
                className="small"
                onClick={() => {
                  if (window.confirm(deleteConfirmText(c.name))) void cats.remove(c.id)
                }}
              >
                削除
              </button>
            </li>
          ))}
        </ul>
      )}

      <h2>退避ファイル</h2>
      {archive == null ? (
        <p className="muted">読み込み中…</p>
      ) : archive.length === 0 ? (
        <p className="muted">退避したファイルはありません（正規化で置き換えた元ファイルがここに載る）</p>
      ) : (
        <table className="history-table archive-table">
          <thead>
            <tr>
              <th className="num">#</th>
              <th>元のパス</th>
              <th>退避先（Archive/）</th>
              <th>理由</th>
              <th>退避</th>
              <th>GC 期限</th>
              <th>状態</th>
              <th>復元</th>
            </tr>
          </thead>
          <tbody>
            {archive.map((a) => (
              <tr key={a.id} className={`state-${a.state}`}>
                <td className="num">{a.id}</td>
                <td className="path">{a.source_rel_path}</td>
                <td className="path">{a.rel_path}</td>
                <td className="nowrap">{archiveReasonLabel(a.reason)}</td>
                <td className="nowrap">{formatDateTime(a.archived_at)}</td>
                <td className="nowrap">{formatDateTime(a.eligible_after)}</td>
                <td className="nowrap">
                  {archiveStateLabel(a.state)}
                  {a.state_at != null && <span className="muted small"> {formatDateTime(a.state_at)}</span>}
                </td>
                <td className="nowrap">
                  {a.state === 'held' && a.batch_id != null ? (
                    <button type="button" className="link small" onClick={() => onOpenBatch(a.batch_id!)}>
                      バッチ #{a.batch_id} を巻き戻す
                    </button>
                  ) : (
                    ''
                  )}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      )}

      <h2>
        config.toml {config?.path && <span className="muted small">{config.path}</span>}
      </h2>
      {config == null ? <p className="muted">読み込み中…</p> : <pre className="config-text">{config.text}</pre>}
      <p className="muted small">変更はファイルを編集してコンテナを再起動する（画面からは書き換えない）</p>
    </section>
  )
}
