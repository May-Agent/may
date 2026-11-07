---
name: new-repo
description: "MUST read before creating a GitHub repository (gh repo create, a new project's first push). Sets the default branch, branch protection, CI jobs, license and README to one fixed standard."
---

# New repository

Follow the steps in order. Each detail file is read at the step that names it; the CI jobs, the protection payload and the README outline are fixed by those files and are not chosen again here.

1. **Ask the user for the license** with `ask_user`, in one question: the SPDX identifier (for example `MIT`, `Apache-2.0`, `MIT OR Apache-2.0`) and the copyright holder name. Do not pick one yourself. Do not continue without the answer.
2. **Create the repository** under the owner the user named, private unless the user said public: `gh repo create OWNER/NAME --private --description "<one line>"`. Create no branch protection and no files through the web editor.
3. **Make the first commit locally**, on a branch named `dev`, containing `LICENSE` (the text of the chosen license with the holder's name and the current year), `README.md` (outline in `references/readme.md`), `.gitignore` for the language, the language's manifest and entry point, and the CI workflow (step 4). Sign the commit. Push `dev` as the first commit of the repository: `git push -u origin dev`.
4. **CI**: copy the template for the project's language and edit only what its `# PROJECT` comments mark.
   - Rust: `references/ci-rust.md`
   - TypeScript with bun: `references/ci-typescript.md`
   - Any other language: ask the user which toolchain commands to use; the minimum is still build, test, lint and format check, each path-filtered, built on the same shape as the templates.
5. **Create `main` from `dev`** and set `dev` as the default branch: `references/protection.md`, section "Branches".
6. **Apply the protection** on `dev` and `main` through the GitHub API exactly as `references/protection.md` gives it, then read it back and compare. A field that differs is a failed step: fix it and read again.
7. **Wait for the first CI run** on `dev` to finish green (`gh run watch`), then add the job names as required status checks as `references/protection.md` section "Required checks" says.
8. **Report** to the user: the repository URL, the default branch, the protection read-back result, the CI job names, the license.

Releases merge `dev` into `main` as a merge commit. After step 3 nothing is pushed to `dev` or `main` directly; every change goes through a pull request.
