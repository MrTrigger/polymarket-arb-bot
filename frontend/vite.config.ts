import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import path from 'path'

// https://vite.dev/config/
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      '@': path.resolve(__dirname, './src'),
    },
  },
  build: {
    // Output directory (relative to frontend/)
    outDir: 'dist',
    // Generate source maps for debugging production issues
    sourcemap: false,
    // Set chunk size warning limit (lightweight-charts is large)
    chunkSizeWarningLimit: 600,
    rollupOptions: {
      output: {
        // Manual chunk splitting for better caching
        manualChunks: {
          // Vendor chunk for React and related libs
          'vendor-react': ['react', 'react-dom', 'react-router-dom'],
          // Charting library (large, rarely changes)
          'vendor-charts': ['lightweight-charts'],
          // State management
          'vendor-state': ['zustand', '@tanstack/react-query'],
        },
      },
    },
  },
  // Base path for assets (adjust if served from subpath)
  base: '/',
})
