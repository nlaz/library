import { defineConfig } from "vite";

export default defineConfig({
  server: {
    port: 5173,
    strictPort: true, // tauri.conf.json points here
    proxy: {
      "/api": "http://127.0.0.1:8080",
      "/pages": "http://127.0.0.1:8080",
    },
  },
});
