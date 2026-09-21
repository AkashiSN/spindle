import { defineConfig } from 'vitest/config'

export default defineConfig({
  test: {
    include: ['src/**/*.test.ts'],
    environment: 'node',
    // index.css を ?raw で読むテスト（lib/theme.test.ts）のため。既定の false だと CSS が空文字に置き換わる
    css: true,
  },
})
