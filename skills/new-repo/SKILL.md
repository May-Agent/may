---
name: new-repo
description: "MUST read before creating a GitHub repository (gh repo create, a new project's first push). Sets the default branch, branch protection, CI jobs, license and README to one fixed standard."
---

# New repository

Follow the steps in order. Each detail file is read at the step that names it; the CI jobs, the protection payload, the release workflow and the README outline are fixed by those files and are not chosen again here.

The repository has one permanent branch — its default — named `main`, or whatever `git config init.defaultBranch` returns when that is set. `<default>` below stands for that name.

1. **Ask the user for the license** with `ask_user`, in one question: the SPDX identifier (for example `MIT`, `Apache-2.0`, `MIT OR Apache-2.0`) and the copyright holder name. Do not pick one yourself. Do not continue without the answer.
2. **Create the repository** under the owner the user named, private unless the user said public: `gh repo create OWNER/NAME --private --description "<one line>"`. Create no branch protection and no files through the web editor.
3. **Make the first commit locally**, on `<default>`, containing:
   - The license file or files: a single SPDX identifier is one `LICENSE`; an expression is one `LICENSE-<ID>` file per operand (`MIT OR Apache-2.0` becomes `LICENSE-MIT` and `LICENSE-APACHE-2.0`). Each file is the canonical text fetched from `https://raw.githubusercontent.com/spdx/license-list-data/main/text/<ID>.txt` with its placeholders filled by the holder's name and the current year.
   - `README.md` (outline in `references/readme.md`), `.gitignore` for the language, the language's manifest and entry point. The manifest records the same answer in its `license` field — `license = "MIT OR Apache-2.0"` in `Cargo.toml`, `"license": "MIT OR Apache-2.0"` in `package.json`.
   - For a Rust project, `deny.toml` as `references/ci-rust.md` gives it, plus `release-plz.toml` and the release workflow as `references/release.md` gives them.
   - The CI workflow (step 4).
   Sign the commit. Push it as the first commit of the repository: `git push -u origin <default>`.
4. **CI**: copy the template for the project's language and edit only what its `# PROJECT` comments mark.
   - Rust: `references/ci-rust.md`
   - TypeScript with bun: `references/ci-typescript.md`
   - Any other language: ask the user which toolchain commands to use; the minimum is still build, test, lint and format check, each path-filtered, built on the same shape as the templates.
5. **Release** (Rust projects): `references/release.md`.
6. **Apply the protection** on `<default>` through the GitHub API exactly as `references/protection.md` gives it, then read it back and compare. A field that differs is a failed step: fix it and read again.
7. **Wait for the first CI run** on `<default>` to finish green (`gh run watch`), then add the job names as required status checks as `references/protection.md` section "Required checks" says.
8. **Report** to the user: the repository URL, the default branch, the protection read-back result, the CI job names, the license.

Releases are release-plz pull requests on the default branch: the action keeps one release PR open (a version bump and a changelog generated from conventional commits), and merging it tags the version, creates the GitHub release and publishes the crate when `publish` is on. After step 3 nothing is pushed to the default branch directly; every change goes through a pull request.
