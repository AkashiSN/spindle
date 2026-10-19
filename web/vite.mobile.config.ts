import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

// スマホ版（/m/）は PC とは別の vite ビルドで dist/m へ出す。
// PC のビルドが dist を空にしてから出力するので、こちらは必ず後に走らせる。
// proxy 設定は vite.config.ts と同じものを複製している（あちらは編集しない）
const backend = process.env.SPINDLE_BACKEND ?? 'http://127.0.0.1:8080'

const proxy = {
  target: backend,
  changeOrigin: true,
  headers: { origin: backend },
}

export default defineConfig({
  root: 'mobile',
  base: '/m/',
  plugins: [react()],
  build: {
    outDir: '../dist/m',
    emptyOutDir: true,
  },
  server: {
    proxy: {
      '/api': proxy,
      '/health': proxy,
    },
  },
})
