// 非同期要求の世代番号。応答が返ったときに「まだ最新の要求か」を見て、古い応答を捨てる
// （A → B の順に出して B → A の順に返っても A を採用しない）。画面を閉じたら invalidate

export class Latest {
  private seq = 0

  /** 新しい要求を始める。戻り値を応答時の isCurrent に渡す */
  next(): number {
    this.seq += 1
    return this.seq
  }

  isCurrent(id: number): boolean {
    return id === this.seq
  }

  /** 進行中の要求をすべて無効にする */
  invalidate(): void {
    this.seq += 1
  }
}
