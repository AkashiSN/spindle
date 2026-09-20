// バッジの凡例（SPEC §12.2）。表の「凡例」ボタンで開くポップオーバー。内容は lib/badges.ts の BADGE_LEGEND

import { BADGE_LEGEND } from '../lib/badges'

export function BadgeLegend() {
  return (
    <div className="badge-legend" role="dialog" aria-label="バッジの凡例">
      {BADGE_LEGEND.map((group) => (
        <section key={group.title}>
          <h4>{group.title}</h4>
          <ul>
            {group.items.map((it, i) => (
              <li key={`${it.key}-${i}`}>
                <span className={it.cls}>{it.icon}</span>
                <span>{it.label}</span>
              </li>
            ))}
          </ul>
        </section>
      ))}
      <p className="muted small">バッジにカーソルを合わせて止めると、その行の詳しい文言が出ます</p>
    </div>
  )
}
