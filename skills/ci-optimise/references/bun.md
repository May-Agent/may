# bun test

`ci slow` reads the `(pass) name [duration]` lines of a `bun test` log and attaches each test to the file heading above it. Bun hides passing tests when it detects a coding agent; the CI log shows them.

- **Slowest tests first.** The slowest entries are usually awaits on a timer, a real network call, or a module that does heavy work at import. Replace fixed timers with the awaited condition, and stub the network at the boundary the test is not about.
- **Shared setup.** Expensive work in `beforeEach` that could run in `beforeAll`, or per-file setup repeated across many files. Move it to a preload (`--preload`) when every file needs it.
- **Import cost.** A file's first test absorbing the load time of a large dependency. Import lazily inside the tests that need it.
- **Install and build steps.** `bun install` without a cache, or a frontend build run before tests that do not need it. Restore the install cache keyed on the lockfile and run the build only in the job that uses its output.
- **Type-check and lint.** These are separate steps with their own timing in `ci slow`; look at them before assuming the tests are the slow part.
