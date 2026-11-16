# cargo test and cargo nextest

`ci slow` reads two things from a `cargo test` log: the per-binary total on each `test result: ... finished in Ns` line, and the `test NAME has been running for over 60 seconds` markers, which name a test that ran at least that long. Per-test times need `-- -Z unstable-options --report-time` on a nightly toolchain. `cargo nextest` logs name every test with its duration and mark tests past the slow period with `SLOW`.

- **One binary dominates.** The total of one test binary is most of the step. Open its slowest tests (nextest names them) and look for sleeps, real network or disk work, and repeated expensive setup that a shared fixture would do once.
- **Compile time inside the step.** If the step's time exceeds the sum of the test binaries, the rest is compiling. Check the cache of `target/` and whether the job builds with a different profile or feature set than an earlier job, which forces a second build.
- **Doc-tests.** Each doc-test compiles separately; a crate with many is slow. Mark examples that need no execution `no_run` or `compile_fail` only where that is what they demonstrate.
- **Parallelism.** `cargo test` runs tests in a binary on threads, binaries one after another; nextest runs every test as its own process across all cores. Moving to nextest is a change of runner, kept only if it lowers the same job's duration.
- **Integration tests as separate binaries.** Every file under `tests/` links its own binary. Many small files cost link time; fold them into one binary with modules.
