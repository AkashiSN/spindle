import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// 開発時はフロントを Vite で配信し、API はローカルで起動した spindle へ中継する。
// 本番は `npm run build` の dist を rust-embed でバイナリへ同梱する
const backend = process.env.SPINDLE_BACKEND ?? 'http://127.0.0.1:8080'

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/api': backend,
      '/health': backend,
    },
  },
})
