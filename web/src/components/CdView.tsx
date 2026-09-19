// CD 画面（SPEC §12.6 のウィザード。P2-3 は「候補選択」まで）。
// 検出（P2-1）が入るまでは TOC を貼り付けて照会する。候補を選ぶとトラック対応を確認できる。
// 吸い出し（P2-5）と手入力（P2-4）は後続

import type { CdLookupState } from '../hooks/useCdLookup'
import { candidateLengthMs, candidateSummary, lookupHeadline } from '../lib/cd'
import { formatDuration } from '../lib/format'

export function CdView({ cd }: { cd: CdLookupState }) {
  const { result, chosen } = cd
  return (
    <section className="cd">
      <div className="table-toolbar">
        <h1>CD</h1>
        <span className="spacer" />
        {result != null && (
          <button type="button" className="ghost" onClick={cd.reset}>
            結果を消す
          </button>
        )}
      </div>

      <h2>TOC</h2>
      <p className="muted small">
        ドライブの検出は未実装（P2-1）。TOC を貼り付けて照会する: CTDB 形式 <code>0:13915:25592:…:リードアウト</code>
        （データトラックは <code>-</code> 前置）、MusicBrainz 形式 <code>1 12 リードアウト+150 各オフセット+150…</code>、
        または <code>cdrecord -toc</code> の出力
      </p>
      <div className="op-row">
        <textarea
          aria-label="TOC"
          rows={3}
          value={cd.toc}
          disabled={cd.busy}
          placeholder="0:22593:41700:58133:71920:91198:104468:115188:131988:143758:159678:174415:191880"
          onChange={(e) => cd.setToc(e.target.value)}
        />
      </div>
      <div className="op-row">
        <button type="button" className="primary" disabled={cd.busy} onClick={() => void cd.lookup()}>
          {cd.busy ? '照会中…' : 'MusicBrainz に照会'}
        </button>
        <span className="muted small">UA 付き・1 秒 1 回。同人・VTuber・インディーズの国内盤は未登録が普通</span>
      </div>
      {cd.error != null && <p className="error">{cd.error}</p>}

      {result != null && (
        <>
          <h2>{lookupHeadline(result)}</h2>
          <dl className="cd-ids small">
            <dt>MusicBrainz DiscID</dt>
            <dd>
              <code>{result.discid}</code>
            </dd>
            <dt>AccurateRip</dt>
            <dd>
              <code>{result.accuraterip_id}</code>
            </dd>
            <dt>CTDB TOCID</dt>
            <dd>
              <code>{result.ctdb_toc_id}</code>
            </dd>
          </dl>
          {result.candidates.length > 0 && (
            <ul className="cd-candidates">
              {result.candidates.map((c, i) => (
                <li key={`${c.release_id}:${c.medium_position}`}>
                  <label>
                    <input
                      type="radio"
                      name="cd-candidate"
                      checked={cd.selected === i}
                      onChange={() => cd.select(i)}
                    />{' '}
                    <strong>{c.artist}</strong> — {c.title}
                    {c.exact ? <span className="badge">DiscID 一致</span> : <span className="badge muted">近似</span>}
                    <div className="muted small">
                      {candidateSummary(c)} · {c.tracks.length} 曲
                      {candidateLengthMs(c) != null ? ` · ${formatDuration(candidateLengthMs(c))}` : ''}
                    </div>
                  </label>
                </li>
              ))}
            </ul>
          )}
        </>
      )}

      {chosen != null && (
        <>
          <h2>
            選択中: {chosen.artist} — {chosen.title}
          </h2>
          <table className="cd-tracks">
            <thead>
              <tr>
                <th>#</th>
                <th>タイトル</th>
                <th>アーティスト</th>
                <th>長さ</th>
                <th>ISRC</th>
              </tr>
            </thead>
            <tbody>
              {chosen.tracks.map((t) => (
                <tr key={t.track_id}>
                  <td>{t.number}</td>
                  <td>{t.title}</td>
                  <td>{t.artist}</td>
                  <td>{formatDuration(t.length_ms)}</td>
                  <td className="muted small">{t.isrcs.join(', ')}</td>
                </tr>
              ))}
            </tbody>
          </table>
          <p className="muted small">
            Release <code>{chosen.release_id}</code>
            {chosen.release_group_id ? (
              <>
                {' '}
                / Release group <code>{chosen.release_group_id}</code>
              </>
            ) : null}
            。吸い出しと配置は P2-5 / P2-8
          </p>
        </>
      )}
    </section>
  )
}
