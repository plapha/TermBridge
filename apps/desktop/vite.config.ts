import { defineConfig } from "vite";

// Tauri 前端构建配置：固定端口，无外部 CDN 依赖。
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
  },
  build: {
    target: "ES2022",
    outDir: "dist",
    emptyOutDir: true,
  },
});
