import react from '@vitejs/plugin-react';
import { defineConfig } from 'vitest/config';

const apiTarget = process.env.HOMELINTER_WEB_PORT ?? 'http://127.0.0.1:8787';

export default defineConfig({
  plugins: [react()],
  base: '/',
  build: {
    target: 'es2022',
    modulePreload: { polyfill: false },
  },
  server: {
    proxy: {
      '/api': {
        target: apiTarget,
        changeOrigin: false,
      },
    },
  },
  test: {
    include: ['src/**/*.test.ts'],
    environment: 'node',
  },
});