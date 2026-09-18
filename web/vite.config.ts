import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// 開発時はフロントを Vite で配信し、API はローカルで起動した spindle へ中継する。
// 本番は `npm run build` の dist を rust-embed でバイナリへ同梱する
const backend = process.env.SPINDLE_BACKEND ?? 'http://127.0.0.1:8080'

// 中継先にはバックエンド自身の Origin / Host を付け直す（CSRF は Origin と Host の完全一致で見る。
// 開発サーバの origin のままだと変更系が 403 になる）
const proxy = {
  target: backend,
  changeOrigin: true,
  headers: { origin: backend },
}

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/api': proxy,
      '/health': proxy,
    },
  },
})
