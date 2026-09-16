import { useEffect, useRef, useState, type Dispatch, type SetStateAction } from 'react'
import { loadJson, saveJson } from '../lib/storage'

/** useState + localStorage 永続化。初回描画では書かず、値が変わったときだけ保存する */
export function useLocalStorageState<T>(
  key: string,
  fallback: T,
  validate?: (v: unknown) => v is T,
): [T, Dispatch<SetStateAction<T>>] {
  const [value, setValue] = useState<T>(() => loadJson(key, fallback, validate))
  const first = useRef(true)
  useEffect(() => {
    if (first.current) {
      first.current = false
      return
    }
    saveJson(key, value)
  }, [key, value])
  return [value, setValue]
}
