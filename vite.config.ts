import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
export default defineConfig({
  plugins: [react()],
  server: { port: 1420, strictPort: true },
  clearScreen: false,
  test: {
    environment: "jsdom",
    maxWorkers: process.env.CI ? 2 : 4,
    setupFiles: ["./src/test-setup.ts"],
    restoreMocks: true,
  },
});
