import { defineConfig } from "vitest/config";
import react from "@vitejs/plugin-react";
export default defineConfig({
  plugins: [react()],
  server: { port: 1420, strictPort: true },
  clearScreen: false,
  test: {
    environment: "jsdom",
    maxWorkers: process.env.CI ? 2 : undefined,
    // Windows CI can spend over ten seconds on the first jsdom/React render.
    // Keep assertion wait limits unchanged; allow only runner startup overhead.
    testTimeout: process.env.CI && process.platform === "win32" ? 15_000 : 5_000,
    setupFiles: ["./src/test-setup.ts"],
    restoreMocks: true,
  },
});
