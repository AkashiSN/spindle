// CD 画面のアルバム欄（P4-20）。毎回使うものだけ表の上に出し、レーベル / カタログ番号 / JAN・UPC と
// トラックリスト貼り付けは「詳細」に畳む（D-72 の「最小限」で写る範囲が上、「全部写す」で埋まる
// 範囲が下、という分け方に合わせてある）。
//
// `CategoryField` は Inbox の承認フォームでも使う（配置先の統制語彙。D-67）

import { useState } from 'react'
import { useCategories } from '../hooks/useCategories'
import type { CdLookupState } from '../hooks/useCdLookup'

export function CdAlbumFields({ cd }: { cd: CdLookupState }) {
  const d = cd.draft
  if (d == null) return null
  const text = (
    label: string,
    key: 'album' | 'album_artist' | 'date' | 'label' | 'catalog_number' | 'barcode',
    hint?: string,
  ) => (
    <label className="cd-field">
      <span>{label}</span>
      <input
        type="text"
        value={d[key]}
        placeholder={hint}
        onChange={(e) => cd.updateDraft({ [key]: e.target.value })}
      />
    </label>
  )
  const num = (label: string, key: 'disc_no' | 'disc_count') => (
    <label className="cd-field cd-field-num">
      <span>{label}</span>
      <input
        type="number"
        min={1}
        max={99}
        value={d[key]}
        onChange={(e) => {
          const v = Number.parseInt(e.target.value, 10)
          if (Number.isFinite(v) && v >= 1) cd.updateDraft({ [key]: v })
        }}
      />
    </label>
  )
  return (
    <>
      <div className="cd-form">
        {text('アルバム', 'album')}
        {text('アルバムアーティスト', 'album_artist')}
        {text('日付', 'date', 'YYYY-MM-DD')}
        {num('ディスク', 'disc_no')}
        {num('枚数', 'disc_count')}
        <CategoryField value={d.category} onChange={(v) => cd.updateDraft({ category: v })} />
      </div>
      <details className="cd-details">
        <summary className="small">詳細（レーベル・JAN/UPC・トラックリスト貼り付け）</summary>
        <div className="cd-form">
          {text('レーベル', 'label')}
          {text('カタログ番号', 'catalog_number')}
          {text('JAN/UPC', 'barcode')}
        </div>
        <h3 className="small">トラックリスト貼り付け</h3>
        <p className="muted small">
          通販ページ等のテキストを 1 行 1 曲で貼る。行頭の番号（<code>1.</code> <code>01</code>{' '}
          <code>M-1</code>）と行末の時間は外し、<code>タイトル / アーティスト</code>（<code>／</code>{' '}
          <code>|</code> <code>-</code> も）で分ける。表（タブ区切り）も可。番号で行に写すので、番号が
          無ければ上から順
        </p>
        <div className="op-row">
          <textarea
            aria-label="トラックリスト"
            rows={6}
            value={cd.paste}
            placeholder={'1. タイトル / アーティスト 4:32\n2. …'}
            onChange={(e) => cd.setPaste(e.target.value)}
          />
        </div>
        <div className="op-row">
          <button type="button" disabled={cd.paste.trim() === ''} onClick={cd.applyPaste}>
            行に写す
          </button>
          <label className="small">
            <input
              type="checkbox"
              checked={cd.pasteArtistFirst}
              onChange={(e) => cd.setPasteArtistFirst(e.target.checked)}
            />{' '}
            アーティスト / タイトル の順で書かれている
          </label>
        </div>
        {cd.pasteWarnings.length > 0 && (
          <ul className="cd-warnings small">
            {cd.pasteWarnings.map((w) => (
              <li key={w}>{w}</li>
            ))}
          </ul>
        )}
      </details>
    </>
  )
}

/** 配置先の category（統制語彙から選ぶ。無ければ _Unsorted。その場で語彙を足せる）。Inbox タブでも使う */
export function CategoryField({
  value,
  onChange,
}: {
  value: string | null
  onChange: (v: string | null) => void
}) {
  const cats = useCategories(true)
  const [adding, setAdding] = useState('')
  const add = async () => {
    const name = adding.trim()
    if (name === '') return
    const c = await cats.create(name)
    if (c != null) {
      onChange(c.name)
      setAdding('')
    }
  }
  return (
    <label className="cd-field cd-field-category">
      <span>category（配置先。未選択なら _Unsorted）</span>
      <select value={value ?? ''} onChange={(e) => onChange(e.target.value === '' ? null : e.target.value)}>
        <option value="">（未分類 → _Unsorted）</option>
        {cats.items.map((c) => (
          <option key={c.id} value={c.name}>
            {c.name}
          </option>
        ))}
      </select>
      <span className="cd-category-add">
        <input
          type="text"
          aria-label="新しい category"
          placeholder="語彙に追加"
          value={adding}
          onChange={(e) => setAdding(e.target.value)}
        />
        <button type="button" disabled={adding.trim() === ''} onClick={add}>
          追加
        </button>
      </span>
      {cats.error != null && <span className="error">{cats.error}</span>}
    </label>
  )
}
