// vite の root（mobile/）の中に置く入口。index.html から root の外（../src/...）を直接指すと
// dev サーバが base の無い /src/... を出して 404 になるので、ここから import する
import '../src/mobile/main.tsx'
