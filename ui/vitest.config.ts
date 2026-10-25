import { defineConfig } from 'vitest/config';
import path from 'node:path';
export default defineConfig({
  resolve: { alias: { '@': path.resolve(import.meta.dirname, 'src') } },
  test: {
    environment: 'jsdom',
    maxWorkers: 1,
    // Thirty seconds leaves scheduling headroom above the measured 20.53s loaded case.
    testTimeout: 30_000,
    setupFiles: ['tests/setup.ts'],
    include: ['tests/**/*.test.{ts,tsx}'],
  },
});
