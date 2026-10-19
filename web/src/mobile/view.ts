// PC 版 / スマホ版の振り分けの Cookie（サーバは spindle_view=pc ならスマホの UA でも / を返す）

export const VIEW_COOKIE = 'spindle_view'

/** 「PC 版を開く」で立てる Cookie（1 年） */
export function pcPreferenceCookie(): string {
  return `${VIEW_COOKIE}=pc; path=/; max-age=31536000; samesite=lax`
}

/** スマホ版を開いたら PC 版の指定を消す（次から / でもスマホ版へ振り分けられる） */
export function clearPcPreference(doc: { cookie: string }): void {
  doc.cookie = `${VIEW_COOKIE}=; path=/; max-age=0; samesite=lax`
}
