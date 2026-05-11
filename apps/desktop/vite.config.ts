import { defineConfig, type PluginOption } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import { fileURLToPath, URL } from 'node:url'

const plugins: PluginOption[] = [
  react() as unknown as PluginOption,
  tailwindcss() as unknown as PluginOption,
]

export default defineConfig({
  plugins,
  build: {
    outDir: 'dist-renderer',
  },
  server: {
    port: 5180,
    strictPort: true,
    fs: {
      allow: ['.', '../../packages/ui'],
    },
  },
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
      '@ui': fileURLToPath(new URL('../../packages/ui/src', import.meta.url)),
    },
    dedupe: ['react', 'react-dom'],
  },
})
