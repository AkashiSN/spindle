// バッジの凡例（SPEC §12.2）。表の「凡例」ボタンで開くポップオーバー。内容は lib/badges.ts の BADGE_LEGEND

import { useLayoutEffect, useRef } from 'react'
import { BADGE_LEGEND } from '../lib/badges'

/** 窓の下端との間に残す余白（px） */
const BOTTOM_GAP = 8

export function BadgeLegend() {
  const ref = useRef<HTMLDivElement>(null)
  // 表のツールバーの下に垂れ下がるので、高さは窓の下端までに収める（固定の 70vh だと、上のプロパティ欄が
  // 高いときに下端がはみ出し、ページはスクロールしないので最後まで読めない）
  useLayoutEffect(() => {
    const el = ref.current
    if (!el) return
    const fit = () => {
      const top = el.getBoundingClientRect().top
      el.style.maxHeight = `${Math.max(120, window.innerHeight - top - BOTTOM_GAP)}px`
    }
    fit()
    window.addEventListener('resize', fit)
    return () => window.removeEventListener('resize', fit)
  }, [])
  return (
    <div ref={ref} className="badge-legend" role="dialog" aria-label="バッジの凡例">
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
