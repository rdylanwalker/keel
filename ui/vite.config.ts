/// <reference types="vitest/config" />
import path from "path"
import { defineConfig } from "vite"
import react from "@vitejs/plugin-react"
import tailwindcss from "@tailwindcss/vite"

export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      "@": path.resolve(import.meta.dirname, "./src"),
      // @rjsf/core's barrel re-exports `getTestRegistry`, which statically imports
      // @rjsf/validator-ajv8 (an rjsf devDependency). AJV compiles with `new Function`, which our
      // CSP forbids, so the package is not installed; point the import at the eval-free validator.
      "@rjsf/validator-ajv8": path.resolve(
        import.meta.dirname,
        "./src/components/rjsf/validator.ts"
      ),
    },
  },
  build: {
    rolldownOptions: {
      output: {
        manualChunks(id: string) {
          if (id.includes("@rjsf") || id.includes("@cfworker/json-schema")) {
            return "vendor-rjsf"
          }
          if (
            id.includes("node_modules/react/") ||
            id.includes("node_modules/react-dom/")
          ) {
            return "vendor-react"
          }
          if (id.includes("lucide-react")) {
            return "vendor-lucide"
          }
        },
      },
    },
  },
  test: {
    globals: true,
    environment: "jsdom",
    setupFiles: ["./src/test/setup.ts"],
    // Inline @rjsf/core so the `@rjsf/validator-ajv8` alias above also applies under vitest.
    server: { deps: { inline: [/@rjsf\/core/] } },
    coverage: {
      provider: "v8",
      reporter: ["text", "json", "html"],
      exclude: [
        "dist/**",
        "node_modules/**",
        "src/test/**",
        "src/main.tsx",
        "src/vite-env.d.ts",
        "src/types/**",
        "**/*.d.ts",
        "**/*.config.*",
      ],
      thresholds: {
        lines: 80,
        functions: 80,
        branches: 80,
        statements: 80,
      },
    },
  },
})
