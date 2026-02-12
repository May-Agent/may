import path from "path"
import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

const backendTarget = process.env.MAY_API_TARGET ?? 'http://127.0.0.1:8787';

// https://vite.dev/config/
export default defineConfig({
  plugins: [react()],
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  server: {
    proxy: {
      '/api': {
        target: backendTarget,
        changeOrigin: true,
      },
    },
  },
})
