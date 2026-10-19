// 横並びの切り替え（ライブラリの アーティスト / アルバム / 曲 など）

export function Segmented<T extends string>({
  options,
  value,
  onChange,
}: {
  options: readonly (readonly [T, string])[]
  value: T
  onChange: (v: T) => void
}) {
  return (
    <div className="m-segmented" role="tablist">
      {options.map(([v, label]) => (
        <button
          key={v}
          type="button"
          role="tab"
          aria-selected={v === value}
          className={v === value ? 'active' : undefined}
          onClick={() => onChange(v)}
        >
          {label}
        </button>
      ))}
    </div>
  )
}
