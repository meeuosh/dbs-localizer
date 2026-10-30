# Zurich import verification — 2026-10-30

Scope: native scan selection, reviewed conversion and import into the existing Localizer dataset format. Contract: [specification 17](../specs/17_zurich_import.md). Imaging and generated datasets remain outside Git.

## Automated checks

- `cargo test --release --offline -p dbsloc-zurich`: 15 tests passed. Includes DICOM header parsing, signed/calibrated pixels, duplicate SOP filtering, numeric age order, chronology, anatomical type families/exclusions, zero-b-value anatomical MRI, invalid acquisition values, metadata scrubbing and owned-output cleanup.
- `cargo test --release --offline -p dbsloc-app import_wizard`: 27 tests passed. Includes the existing import flow, Zurich prepared-pair import through the real writer, review/type override, explicit subject ID, distinct unnamed candidates, cancellation and temporary-directory ownership.
- `cargo build --release --offline -p dbsloc-app`: passed.
- `cargo clippy --release --offline -p dbsloc-zurich -p dbsloc-app --all-targets -- -D warnings`: passed.
- `cargo fmt -p dbsloc-zurich -p dbsloc-app -- --check`: passed.

## Supplied hospital export

Read-only scan found 37,616 candidate files and 140 CT/MR series across five CT sessions and three MR sessions. Automatic suggestions selected:

| Target | Session | Slices | Voxel spacing (mm) | Type/evidence |
|---|---|---:|---|---|
| Pre-op MRI | `MR/export-003` | 168 | 1 × 1 × 1 | T1w parameter guess: TR 10.62 ms, TE 6.005 ms, 3D gradient echo; date predates positive CT evidence |
| Post-op CT | `CT/export-004` | 309 | 0.553 × 0.553 × 0.5 | Sparse intracranial metal signature; the older session 5 had no qualifying signature |

The export carries a zero DiffusionBValue on ordinary anatomical MRI. A regression fixture verifies that zero alone does not exclude anatomical type inference; positive diffusion or corroborating diffusion/EPI flags still do.

The inspection helper prepared the suggested pair with no manual type/index override, imported it into a temporary dataset and resolved the result using `dbsloc_pipeline::bids::resolve_subject_with`. The command exited successfully. The dataset contained exactly one pre-op T1w and one post-op CT volume with their sidecars/index. Both NIfTI files retained qform/sform codes 1; free-text header fields and extension flags were cleared. Sidecars contained only allowed acquisition fields and generic modality. No `sourcedata` archive was created.

These checks verify import behavior. MRI contrast and electrode evidence remain reviewable heuristics; registration and reconstruction were outside this verification scope.

## Native window

The release binary was exercised with the existing screenshot mode under a temporary Xvfb display and software Vulkan renderer. Screenshot capture waits for import discovery to finish; source data and the screenshot remain local temporary files. The panel shows separate suggested MRI/CT candidates, middle-slice previews, type correction, evidence and a mandatory review checkbox before conversion.
