import path from 'path';
import tailwindcss from '@tailwindcss/vite';
import react from '@vitejs/plugin-react';
import { defineConfig } from 'vite';
import { viteSingleFile } from 'vite-plugin-singlefile';

// One self-contained dist/index.html so the built cockpit opens straight from disk (file://).
export default defineConfig({
  base: './',
  plugins: [react(), tailwindcss(), viteSingleFile()],
  resolve: { alias: { '@': path.resolve(import.meta.dirname, './src') } },
});
