// 下のタブ（ライブラリ / フォルダ / プレイリスト / 購読）

import { TABS, type Tab } from '../route'

export function TabBar({ current, onSelect }: { current: Tab; onSelect: (t: Tab) => void }) {
  return (
    <nav className="m-tabbar">
      {TABS.map(([tab, label]) => (
        <button
          key={tab}
          type="button"
          className={tab === current ? 'active' : undefined}
          aria-current={tab === current ? 'page' : undefined}
          onClick={() => onSelect(tab)}
        >
          {label}
        </button>
      ))}
    </nav>
  )
}
