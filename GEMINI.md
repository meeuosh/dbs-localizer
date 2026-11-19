# Zurich Fork Context — Wyss DBS Localizer

> This file supplements `AGENTS.md` for the personal Zurich fork (`meeuosh/dbs-localizer`). Keeping fork-specific rules here avoids merge conflicts when pulling upstream updates from `WyssCenter/dbs-localizer`.

## Chat Style
- Use **normal, clear prose** (overrides the `caveman full` chat style in `AGENTS.md`).

## Git & Fork Workflow
- **Remotes:** `upstream` = `https://github.com/WyssCenter/dbs-localizer.git` (official repo), `origin` = `https://github.com/meeuosh/dbs-localizer.git` (personal fork).
- **Branches:**
  - `main`: clean, untouched mirror of `upstream/main` (`v0.2.0+`). Never commit Zurich changes to `main`.
  - `zurich-import`: active development branch containing all upstream SOTA features plus the Zurich hospital-folder import extension.
- **Syncing upstream updates:** `git fetch upstream && git merge upstream/main` (while on `zurich-import`), verify tests, then `git push`.

## Zurich Extension Architecture & Scope
- **Goal:** Retain full SOTA functionality of Wyss DBS Localizer while accelerating and tailoring patient data import for Zurich hospital export folders (`CT/export-*`, `MR/export-*` or `MRI/export-*`).
- **Isolation rule:** Keep Zurich-specific logic isolated inside:
  - `crates/dbsloc-zurich/` (DICOM triage, MRI contrast heuristics, metadata scrubbing, preparation, and helper examples `inspect`, `diagnose`, `cohort`).
  - `crates/dbsloc-app/src/import_wizard/zurich.rs` (native GUI panel).
  - Touch upstream files (`Cargo.toml`, `crates/dbsloc-app/Cargo.toml`, `crates/dbsloc-app/src/import_wizard.rs`, `crates/dbsloc-app/src/app.rs`) only for minimal wiring hooks so upstream merges stay clean.
- **Canonical docs:** `specs/17_zurich_import.md`, `docs/ZURICH_IMPORT_VALIDATION.md`, `docs/GIT_WORKFLOW.md`.

## Two-Machine Workflow (Linux Dev → Windows Clinical Data)
- Development and synthetic-data testing happen on **Linux**; real patient data lives on a **Windows PC** that pulls `zurich-import` from `origin` and compiles from source (`cargo build --release -p dbsloc-app -p dbsloc-cli`).
- **Windows portability:** Always use `dbsloc_zurich::canonical_path` (which strips Windows `\\?\` verbatim drive/UNC prefixes) before passing paths to external tools (`dcm2niix.exe`, `dbsloc.exe`). Keep raw `std::fs::canonicalize` only for direct Rust filesystem reads/copies (e.g., deeply nested hospital DICOM slices).
- **Privacy:** Never commit patient data, DICOMs, NIfTIs, or local dataset/cohort logs to Git.

## v0.2.0 Crate & License Hygiene
- Every new Rust file in `crates/dbsloc-zurich/` and `crates/dbsloc-app/src/import_wizard/zurich.rs` must start with:
  ```rust
  // SPDX-FileCopyrightText: 2026 Wyss Center for Bio and Neuroengineering
  // SPDX-License-Identifier: GPL-3.0-only
  ```
- Keep `publish = false`, `license = "GPL-3.0-only"`, and `dbsloc-pipeline = { path = "../dbsloc-pipeline", default-features = false }` in `crates/dbsloc-zurich/Cargo.toml` so no-PaCER GPL-3.0-only builds stay valid.
- Focused verification commands:
  - `cargo test -p dbsloc-zurich --all-targets`
  - `cargo test -p dbsloc-app import_wizard`
  - `cargo clippy -p dbsloc-zurich -p dbsloc-app --all-targets -- -D warnings`
  - `cargo fmt -p dbsloc-zurich -p dbsloc-app -- --check`
