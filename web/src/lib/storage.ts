// localStorage の読み書き。列設定・パネル幅などの「消えても困らない」値だけを置く。
// private window や容量超過で例外になるので、失敗は黙って既定値に倒す

const PREFIX = 'spindle:'

export function loadJson<T>(key: string, fallback: T, validate?: (v: unknown) => v is T): T {
  try {
    const raw = localStorage.getItem(PREFIX + key)
    if (raw == null) return fallback
    const parsed: unknown = JSON.parse(raw)
    if (validate && !validate(parsed)) return fallback
    return parsed as T
  } catch {
    return fallback
  }
}

export function saveJson(key: string, value: unknown): void {
  try {
    localStorage.setItem(PREFIX + key, JSON.stringify(value))
  } catch {
    // 保存できなくても動作には影響しない
  }
}
