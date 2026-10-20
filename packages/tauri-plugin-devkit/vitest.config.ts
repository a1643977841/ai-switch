import { defineConfig } from "vitest/config";

export default defineConfig({
  test: {
    environment: "node",
    include: ["tests/**/*.test.ts"],
    exclude: ["tests/browser/**", "tests/cli-e2e.test.ts"],
    // These suites pack template archives and write real files to the temp
    // directory, and Windows CI runs them on a slower, virus-scanned disk. The
    // 5000ms default is too tight for that: `archive-pack.test.ts` and
    // `init.test.ts` each timed out on a different Windows run while passing
    // locally, so the release pipeline failed on tests that were never broken.
    // The budget is a ceiling, not a target — a passing run stays well under it.
    testTimeout: 30_000,
    hookTimeout: 30_000,
  },
});
