---
name: ci-optimise
description: Read when a CI alert says a job ran abnormally long, or before changing CI or a test suite to make a run faster. Gives the procedure for finding where a slow run spends its time and proving the fix.
---

# Making a slow CI run faster

A slow job slows every verification and feedback cycle and wastes compute. Work through the steps in order; the result is a measured drop in the same job's duration.

## 1. Find where the time goes

- `ci slow <pr>` breaks the PR's checks down by job, step and test against each job's baseline (`ci slow <run> --run` does the same for a recorded workflow run). Start from the job the alert names, then its slowest step, then its slowest tests.
- `ci stats` shows each job's latest, median and p90 duration and whether it is getting slower: a steady climb points at growth (more tests, a larger build), a jump at one run points at a change or a cache miss.
- `ci queue` compares the merge-queue wait with the run time. A long wait behind a short run is queue length, not the run; a long run is the job.

Name the one step or test that holds most of the time before changing anything.

## 2. Find the cause

Read the log of that step (`gh run view --job <id> --log`) and match it to the usual causes in `references/causes.md`: cold or missing caches, work repeated across jobs, serial work that can run in parallel, a build that compiles more than the job needs, one test dominating the run, waits and timeouts. For the test runner in use, `references/cargo.md` covers cargo test and nextest and `references/bun.md` covers bun test.

State the cause as a measurement ("the cache restore missed and `cargo build` took 11m of 14m"), not a guess.

## 3. Prove the fix

1. Note the job's duration and baseline from `ci slow` before the change.
2. Make the change and push it; the PR's checks run again and the new durations are recorded.
3. Run `ci slow <pr>` again and compare the same job, the same step and the same tests with the first reading. The fix is proven when that job is faster by roughly what the cause predicted; report both numbers.

If the duration did not move, the cause was wrong: go back to step 2.

## What does not make a run faster

Sharding, removing tests, loosening assertions and skipping platforms or configurations only move or hide the cost, or give up coverage. They are not fixes; do not propose them and do not count a drop they produce as one.
