# Keeping a Zurich extension alongside collaborator updates

A **clone** is the repository on your computer. A **fork** is your linked copy on GitHub. A **branch** holds a line of work inside a repository. A **commit** records a recoverable version of your changes. A **remote** is a named GitHub address.

Use a private fork when the source repository permits it. Keep the repository's existing licence and notices. GitHub's private-fork availability depends on repository settings and account permissions; see [GitHub's fork documentation](https://docs.github.com/en/pull-requests/reference/forks). If the Fork button is unavailable, agree on a private shared branch or another approved private repository with the owner.

## Current local setup

The original checkout points `origin` at `richardkoehler/dbs-localizer`. The Zurich extension is developed on `codex/zurich-import`; creating that local branch does not create a GitHub fork or publish changes. The separate Zurich Convert directory is not currently a Git repository.

## Connect your fork once it exists

Create the fork using GitHub's **Fork** button. Then, from this checkout, replace `YOUR-USERNAME` below with your GitHub account:

```bash
git remote rename origin upstream
git remote add origin https://github.com/YOUR-USERNAME/dbs-localizer.git
git remote -v
```

Now `origin` means your fork and `upstream` means the collaborator's repository. These commands change addresses, not source files. Run them once; do not repeat the rename on an already configured checkout.

Review and commit the code before the first push. Stage named source/documentation files only; patient data, datasets, temporary outputs and local logs must stay outside Git. `git status` and `git diff --cached` show exactly what a commit will contain. Then publish the committed feature branch:

```bash
git push -u origin codex/zurich-import
```

Pushing backs up committed work to your fork. It does not change the collaborator's repository. A pull request is a separate request to merge a branch; choose its target deliberately.

## Bring in collaborator updates

Start with committed work and a clean `git status`. Then:

```bash
git fetch upstream
git switch codex/zurich-import
git merge upstream/main
cargo test --release -p dbsloc-zurich
cargo test --release -p dbsloc-app import_wizard
cargo build -p dbsloc-app --release
git push origin codex/zurich-import
```

`fetch` downloads version history without editing your files. `merge` combines the collaborator's latest `main` with your branch. Your commits remain in history. If both versions changed the same lines, Git may stop for a conflict; review the affected files, resolve them, then finish the merge. `git merge --abort` cancels an unfinished merge and returns to the pre-merge state. Do not discard changes with `reset --hard` to fix a conflict.

Ordinary `git pull` follows the current branch's configured remote. After the first push above, it pulls your feature branch from your fork; it does not automatically merge the collaborator's `main`. Use the explicit update sequence above.

Git keeps source code. Rebuild after updates to get an executable containing your extension. An installer downloaded from the collaborator contains their released code; it will include your extension only if they have merged and released it. Your own fork can also have its own builds/releases later.

## Why the extension is separate

Zurich-specific code lives in `crates/dbsloc-zurich/` and the native GUI child module `crates/dbsloc-app/src/import_wizard/zurich.rs`. Shared changes are limited to dependency declarations, small wizard hooks and documentation. This reduces overlapping edits; it cannot guarantee conflict-free updates or protect against future upstream API changes. Rebuild and run the focused checks after merging updates. A standalone API contract lives in [the Zurich import specification](../specs/17_zurich_import.md).

The cleanest long-term arrangement is to offer the small extension hooks upstream if the collaborator wants them. Then subsequent work can stay mostly in the Zurich files.
