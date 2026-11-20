---
name: new-repo
description: "MUST read before creating a GitHub repository (gh repo create, a new project's first push). Sets the default branch, branch protection, CI jobs, license and README to one fixed standard."
---

# New repository

Follow the steps in order. Each detail file is read at the step that names it; the CI jobs, the protection payload and the README outline are fixed by those files and are not chosen again here.

1. **Ask the user for the license** with `ask_user`, in one question: the SPDX identifier (for example `MIT`, `Apache-2.0`, `MIT OR Apache-2.0`) and the copyright holder name. Do not pick one yourself. Do not continue without the answer.
2. **Create the repository** under the owner the user named, private unless the user said public: `gh repo create OWNER/NAME --private --description "<one line>"`. Create no branch protection and no files through the web editor.
3. **Make the first commit locally**, on a branch named `main`, containing:
   - The license file or files: a single SPDX identifier is one `LICENSE`; an expression is one `LICENSE-<ID>` file per operand (`MIT OR Apache-2.0` becomes `LICENSE-MIT` and `LICENSE-APACHE-2.0`). Each file is the canonical text fetched from `https://raw.githubusercontent.com/spdx/license-list-data/main/text/<ID>.txt` with its placeholders filled by the holder's name and the current year.
   - `README.md` (outline in `references/readme.md`), `.gitignore` for the language, the language's manifest and entry point. The manifest records the same answer in its `license` field — `license = "MIT OR Apache-2.0"` in `Cargo.toml`, `"license": "MIT OR Apache-2.0"` in `package.json`.
   - For a Rust project, `deny.toml`, `release-plz.toml` and `.github/workflows/release-plz.yml` as `references/ci-rust.md` gives them, and `publish = false` in `Cargo.toml` unless the user explicitly asked to publish to crates.io — crates.io publication is irreversible.
   - The CI workflow (step 4).
   Sign the commit. Push `main` as the first commit of the repository: `git push -u origin main`.
4. **CI**: copy the template for the project's language and edit only what its `# PROJECT` comments mark.
   - Rust: `references/ci-rust.md`
   - TypeScript with bun: `references/ci-typescript.md`
   - Any other language: ask the user which toolchain commands to use; the minimum is still build, test, lint and format check, each path-filtered and gathered by the always-running `CI result` job, built on the same shape as the templates.
5. **Set `main` as the default branch**: `references/protection.md`, section "Branches".
6. **Apply the protection** on `main` through the GitHub API exactly as `references/protection.md` gives it — `merge_queue` included, with the documented retry without it when the owner or plan rejects it — then read it back and compare. A field that differs is a failed step: fix it and read again.
7. **For a Rust project, enable the release flow**: the release-plz workflow opens pull requests with `GITHUB_TOKEN`, which needs the repository permission `references/ci-rust.md`'s "Releases" section shows: `gh api -X PUT repos/OWNER/NAME/actions/permissions/workflow -f default_workflow_permissions=read -F can_approve_pull_request_reviews=true`.
8. **Wait for the first CI run** on `main` to finish green (`gh run watch`), then require the `CI result` check as `references/protection.md` section "Required checks" says.
9. **Report** to the user: the repository URL, the default branch, the protection read-back result (including whether `merge_queue` was applied or omitted for plan limits), the CI job names, the license.

`main` is the only trunk. After step 3 nothing is pushed to `main` directly; every change goes through a pull request into `main`. For a Rust project a release is cut by merging release-plz's release PR into `main` — a step that stays the user's instruction — and crates.io is never published to without the user's explicit authorization.
