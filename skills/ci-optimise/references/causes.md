# Usual causes of a slow job

Check each against the slow step's log and its timing in `ci slow`.

- **Cache miss or no cache.** A restore step reporting no hit, or a key that changes every run (a hash of a file that changes on each commit, a timestamp). Fix the key so unchanged inputs hit, and restore the dependency and build-output directories the job actually reuses.
- **Cache too large or restored needlessly.** A restore or save step taking minutes. Cache only what costs more to rebuild than to download.
- **Work repeated across jobs.** Several jobs building or installing the same thing. Build once and pass the artifact, or share a cache between them.
- **Serial steps that are independent.** Lint, type-check and build run one after another in one job. Run them as separate jobs, or in parallel within the job.
- **Building more than the job needs.** A full workspace build for a job that tests one crate, a release profile where the check does not depend on optimisation, all features when one set is checked.
- **Slow tool installation.** Compiling a tool from source on each run. Use a prebuilt binary or a cached install.
- **One test dominating.** A single test or test binary holding most of the step. Look at what it waits on or recomputes (see the framework references) and fix that.
- **Fixed sleeps and long timeouts.** A test that sleeps a fixed time instead of waiting for the condition, or a retry that burns its full timeout on every run.
- **Network in the hot path.** Downloading fixtures, pulling images or fetching dependencies during the step. Cache them, pin them, or serve them locally.
- **Runner mismatch.** A job on a small or shared runner doing parallel work it cannot run in parallel. Match the parallelism to the runner's cores.
