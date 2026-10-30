# Zurich hospital-folder import extension

Decision: 2026-10-30. This optional workflow extends the existing native image-import wizard; ordinary DICOM/NIfTI import, pipeline execution and output naming remain owned by `dbsloc-pipeline::import`.

## 1. Layout and isolation

Implement hospital-specific discovery, evidence and selection in a new `dbsloc-zurich` Rust crate. Keep the GUI in `crates/dbsloc-app/src/import_wizard/zurich.rs`, with small hooks in the existing wizard. No runtime dependency on a sibling checkout, MATLAB, Python or Zurich Convert's older egui version. Zurich Convert supplies the workflow requirements; independently implement against this specification using the existing DICOM Rust ecosystem. No Lead-DBS source may be read.

Supported input: one patient directory containing case-insensitive `CT/` and `MR/` or `MRI/`, each with positive numeric session names such as `001`, `1`, `export-001`. Parse the whole numeric suffix, not lexical order: 1 is newest, larger numbers are older. Unrecognized folders and bundled hospital-viewer directories are ignored. Recursively discover DICOM inside each recognized session, including extensionless slices; never follow symlinks. Source files are read-only. Reject `..` path components and canonicalize the selected patient directory. Group slices by session path and SeriesInstanceUID; do not mix acquisitions across sessions.

## 2. Discovery and suggestions

Read metadata only up to PixelData in parallel, then decode one middle-slice preview and a sparse CT sample. Preserve internal paths/UIDs only in memory. Expose progress counters and cancellation; unreadable images cannot count as electrode-negative evidence. Extract modality, description/protocol/sequence name, study date, dimensions, pixel spacing, thickness, orientation/position, instance number, rescale slope/intercept, image type and frame count. Deduplicate copied SOPInstanceUIDs within each session/series. Unsupported multiframe series are visibly ineligible for automatic selection. Sort slices spatially with instance-number fallback. Guard dimensions before pixel allocations. Respect DICOM signed pixel representation and apply rescale exactly once.

Scouts, localizers, dose reports, screenshots, projections and non-volume stacks are excluded from automatic selection. CT heuristic: sample five slices over 45–80% of the spatial stack, count pixels >=2500 HU in the central 25–75% rectangle, suggest metal present only when localized counts (2–450 pixels) occur on at least two samples. This is a heuristic requiring human review; it does not prove an implanted lead. Decode failure gives unknown evidence. MRI post-op/DBS/electrode metadata rules out an automatic pre-op suggestion. Recognizable anatomical MRI acquired strictly before the earliest CT with positive electrode evidence can be suggested as pre-op; explicit pre-op metadata is weaker supporting evidence. Missing/conflicting evidence remains unknown. The UI must never present absence of a keyword as proof of no electrode.

Select the highest numbered *eligible* session first, then prefer recognizable T1 anatomical MRI (other supported anatomical suffixes permitted), quality (slice thickness/count, excluding scouts), and a stable UID tie-break. Never substitute a newer session solely for a higher quality score. Choose exactly two primary targets: pre-op MRI and post-op CT. Do not auto-import optional pre-op CT or post-op MRI. No eligible candidate leaves the corresponding selection empty. `eligible` means a supported non-scout volume; automatic MRI selection additionally requires an inferred supported anatomical suffix and pre-op evidence. Permit explicit human selection of other supported anatomical volumes, with evidence shown. When the MRI suffix cannot be inferred, leave it unassigned and offer a manual anatomical type selector; pass the explicitly reviewed suffix on a cloned Scan to preparation. A reviewed selection must have exactly one MR and one CT, different candidate indices, and a supported anatomical MRI suffix. All suggestions require user review before conversion/import.

### 2a. Automatic modality/type guessing (user request, 2026-10-30)

CT versus MRI comes from the DICOM Modality tag, with the recognized parent modality folder as a fallback. Anatomical MRI type is a best-effort, reviewable guess, not a diagnostic assertion. Each candidate exposes `suffix_reason: String`; the UI shows why its type was suggested and always permits correction. Never use patient names or original patient identifiers for classification.

First use recognizable SeriesDescription, ProtocolName and SequenceName tokens. Match specific types before broad ones: FGATIR, FLAIR, T2starw/T2*, PDw, T2w, then T1w/MPRAGE/SPGR/BRAVO/TFE/MP2RAGE. Exclude diffusion/DWI/DTI/ADC, perfusion/ASL/DSC, functional/BOLD, field maps, phase and susceptibility/SWI series from automatic anatomical assignment, including their known DICOM acquisition flags when names are absent (including ASLContrast and EPI acquisition flags). These exclusions must also prevent parameter-based misclassification. A steady-state SequenceVariant flag (`SS`) leaves parameter-only anatomical assignment unknown; short TR/TE alone must not turn CISS/FIESTA-like acquisitions into T1w.

The sample hospital export supplies a zero-valued DiffusionBValue on ordinary anatomical MRI. Therefore zero b-value alone, without direction/diffusion/EPI evidence, is not an exclusion. Positive finite b-value, explicit non-`NONE` diffusion direction, EPI flags or diffusion image-type/name tokens exclude automatic anatomical assignment. A b=0 diffusion/EPI acquisition remains excluded when that corroborating evidence exists. Cover this distinction with a real DICOM-header regression fixture.

If names are inconclusive, use finite positive DICOM RepetitionTime (TR), EchoTime (TE) and InversionTime (TI), all in milliseconds, plus ScanningSequence/MRAcquisitionType/FlipAngle when available. Parameter-only guesses require TR in 1–60000 ms, TE in 0.1–1000 ms and TE <=TR; a present positive TI must be <=TR and <=20000 ms. Malformed/nonfinite supplied parameters must not be silently treated as missing. These bounds intentionally leave unusual/ambiguous acquisitions unassigned and guard against second-valued numbers misread as milliseconds. Use these conservative heuristics with the actual measurements in the reason:

- FLAIR: TR >=4000, TE >=60, TI >=1500.
- T1w inversion recovery: TI between 200 and 1400 inclusive, TE <=15 and TR >=TI. A known FLAIR/FGATIR name wins over this broad guess.
- T2w: TR >=1500, TE >=60 and no positive TI. If ScanningSequence is explicitly GR/gradient echo, leave unknown instead.
- PDw: TR >=1500, TE <=30, no positive TI and an explicitly spin-echo (SE) sequence; leave unknown if acquisition family is missing.
- T1w short-TR spin echo: TR between 100 and 1000 inclusive, TE <=30, no positive TI and ScanningSequence explicitly SE.
- T1w short-echo 3D gradient echo: TR <=50, TE <=10, no positive TI, MRAcquisitionType 3D and explicitly GR. Do not guess this for 2D functional scans.

Missing, contradictory or out-of-range parameters leave the type unassigned. Parameter guesses do not change electrode evidence or establish that a scan is pre-operative. Automatic pre-op selection still requires independent pre-op evidence, supported geometry and oldest-session ordering. Tests must cover each accepted family, exclusion precedence, ambiguous/missing parameters, milliseconds, nonfinite values and editable GUI override. Real-data validation must report whether a type was inferred or explicitly assigned.

## 3. Public Rust interface

The backend crate exposes:

```text
Modality { Ct, Mr }
ElectrodeEvidence { Detected, LikelyAbsent, Unknown }
Preview { width: usize, height: usize, pixels: Vec<f32> }
Candidate { session_number: u32, session_path: PathBuf, series_uid: String,
            modality: Modality, description: String, study_date: Option<String>,
            slice_count: usize, spacing_mm: [f32; 3], evidence: ElectrodeEvidence,
            reason: String, eligible: bool, suffix: Option<String>, suffix_reason: String,
            preview: Option<Preview>, ... }
Scan { candidates: Vec<Candidate>, preop_mri: Option<usize>, postop_ct: Option<usize>, warnings: Vec<String> }
Progress { done: usize, total: usize, label: String }
scan_patient(path: &Path, cancel: &AtomicBool, progress: impl Fn(Progress) + Sync) -> Result<Scan, Error>
prepare_selection(scan: &Scan, mri: usize, ct: usize, output: &Path,
                  cancel: &AtomicBool, progress: impl Fn(Progress) + Sync) -> Result<Vec<ImportItem>, Error>
```

The implementation may add fields/helpers. Error is typed with displayable context. `prepare_selection` requires a fresh, nonexisting temporary output directory, owns its cleanup on failure, stages only the selected slices in separate temporary DICOM directories using generic numbered filenames, calls the existing bundled dcm2niix conversion, and returns exactly two explicitly assigned ImportItems. Reject zero or multiple resulting volumes for either target instead of silently picking one. Use preop + detected/reviewed anatomical suffix, with MRI orientation from existing import geometry suggestions when available; use postop + CT with `acq: None` (the existing importer disallows a CT acquisition entity). No raw DICOM is archived by this extension. Remove staged DICOM after conversion.

Converted sidecars use an allowlist of acquisition geometry/scanner/timing numeric values and recognized generic modality fields; never pass through patient identity, original folder names, series/protocol descriptions, UIDs, exact acquisition dates or free text. Clear NIfTI free-text fields and extensions before final import, while preserving voxel data and geometry. This minimizes metadata disclosure and does not claim to de-identify facial anatomy. Keep source labels/dates in the local review view only. Final BIDS subject ID is explicitly entered by the user, never inferred from a hospital identifier.

## 4. Native GUI

In Images step add **Zurich patient folder…**. Background picker/discovery shows progress and cancellation. Within the wizard show candidate session, modality, date, description, slice count, spacing, evidence/reason and middle-slice preview. Two selectors default to suggestions, permit review/override; show **Review scan suggestions before importing**. **Use selected scans** converts only the chosen two on a worker, then inserts explicit rows into the ordinary wizard. Continue through existing subject/destination and final review/import. Source archiving defaults off for this route. Require an explicitly entered subject ID. Reuse existing three-plane NIfTI preview after conversion. Failed conversion must not add partial rows. Disable conflicting wizard controls while a Zurich job runs. Closing/cancelling cancels workers; a worker owns temporary output until handed to wizard, so it cannot leak directories if receiver disappears.

## 5. Verification

Tests cover numeric ordering (including 2 versus 10 and `export-001`), modality aliases, ignored viewers/symlinks, scouts, unknown versus negative evidence, oldest-session priority over quality, MRI date contradictions, deterministic ties, invalid selections, cancellation and conversion cleanup. Build native app and run focused tests. Run scanner and prepare/import into a temporary dataset for the supplied patient, then resolve the result through the existing BIDS resolver. Patient data and generated datasets stay outside Git. GUI layout must be rendered/inspected when display facilities allow. No remote services receive imaging.

## 6. Explicitly authorized unattended cohort (2026-10-30)

### Owner amendment: alternative scans and immediate cohort retry

This amendment supersedes the earlier unattended cohort's strict date-evidence eligibility and stop-after-first-pair behavior. It does not alter native GUI review or anatomical-type inference. The owner confirmed date anonymization; differences in StudyDate cannot establish that the folder convention or true acquisition chronology is wrong without knowing the date-shift scheme.

On conversion or pipeline/localization failure, try a bounded sequence of different valid scan pairs instead of failing at the initial choice. Maximum eight pair attempts by default; configurable positive `--max-attempts`. Try different CT sessions before repeated reconstructions of the same series; vary MRI after CT alternatives. Deduplicate pairs by session path and series UID. CT candidates must be supported/eligible and metal-positive; MRI must be supported/eligible with a recognized anatomical suffix and no explicit post-op/DBS evidence. Anonymized StudyDate is advisory in this unattended route: a date-based Unknown MRI is allowed with explicit pending-review audit, never called proven pre-op. Unassigned MRI types remain unassigned; no blind T1 substitution. Native GUI behavior stays unchanged.

Each pair runs import and processing in an isolated attempt dataset under the batch log directory. Preserve failed attempts and their audit/errors. Import extra supported MRI contrasts per existing rules, allowing advisory-date candidates without explicit post-op evidence. Use the matching shipped T2 template for T2-only anchor attempts. A successful attempt requires pipeline exit zero, reconstruction with both sides traced, and review images generated; do not loosen reconstruction thresholds or approve results.

To bound disk use, preserve failed attempts' scan audits, pipeline logs/results and copied QC PNGs, then remove only their uniquely owned temporary attempt dataset. Record that failed volumetric intermediates were not retained; original DICOM always remains untouched. Keep successful volumes until publication/rollback completes. No unrelated or canonical directories may be cleaned by this path.

Normal invocation still skips existing subjects. Explicit `--retry-failed` permits retries for existing subjects without a successful reconstruction; never replace an existing reconstruction or approved/manual results. For a successful retry, retain old failed raw/derivative directories in a backup before publishing the complete successful subject. Rewrite dataset-root references in staged text/JSON/MAT files to their final locations (or rerun the existing native pipeline's export/visualization using final paths); verify resulting scene/reconstruction/QC references exist. Roll back publication on error. Existing successful subjects and 000003 stay untouched. Persist per-attempt outcomes, selected UIDs/sessions, untrusted-date warnings and final success/skipped/exhausted state. Add tests using a fake CLI for first-pair failure followed by alternative success, exhaustion, no repeated pair, and preservation of completed subjects. Then execute on the supplied cohort as requested, retaining real failures honestly.

Malformed optional converter sidecars must not block a valid NIfTI import: discard all fields from that sidecar and write only generic Modality. Never repair or pass through malformed free text; retain NIfTI integrity checks. This handles hospital protocol text containing unescaped control characters.

The owner authorized processing all remaining `SUBJ-<digits>` directories in the supplied cohort into the existing dataset, excluding subject 000003 and retaining all existing subject data. A standalone `cohort` example in this crate may orchestrate the existing scanner, preparation/import writer and native `dbsloc run` binary. This does not change the GUI's reviewed-pair workflow.

Require explicit source root, dataset root and CLI binary arguments. Subjects derive only from the numeric pseudonymous folder label; reject other names. Skip existing subjects, missing suggested MR/CT pairs and unreadable patients, recording reasons and continuing. For each additional supported anatomical MR suffix, select at most one eligible LikelyAbsent candidate using oldest-session, quality and stable UID order; prefer the primary MRI session when available, then other independently pre-op sessions. Do not import post-op, unknown-evidence, unsupported or unassigned-type MRI. Failed optional MR conversion warns and skips that contrast without discarding a usable primary pair. Reuse `prepare_selection` for sanitization; repeated CT preparation for optional MR is acceptable initially. Import all chosen items in one existing import transaction without replacement or raw DICOM archives.

Run patients sequentially with Medtronic 3389, backend auto, native template/grid defaults, multispectral normalization when T2w is available, fingerprint locks, headless, no scene window and checkreg gating off. Stages: bids, preprocess, precoreg, coreg, normalize, brainshift, reconstruct, visualize. Brainshift retains its existing skip behavior when no supported correction is configured. No stimulation calculation or directional roll estimation is requested for this non-directional model. Generate combined coregistration/normalization review slides and manifest afterwards via the existing checkreg writer; never mark images or reconstructions manually approved. Retain per-patient pipeline JSON, stderr, scan-choice audit (session, series UID, suffix, spacing and evidence/reasons), cohort summary and skip/failure details in the dataset's local batch log directory. No data goes into Git. Persist reports after each patient so interrupted work remains auditable; skip existing outputs on repeat. Tests cover extra-contrast choice, exclusion of unknown/post-op evidence, subject-name parsing and command flags. Actual cohort execution is the primary end-to-end check.
