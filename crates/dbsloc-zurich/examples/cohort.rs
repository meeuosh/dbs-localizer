// SPDX-FileCopyrightText: 2026 Wyss Center for Bio and Neuroengineering
// SPDX-License-Identifier: GPL-3.0-only

//! Explicitly authorized unattended workflow; specs/17 §6.
use std::collections::BTreeMap;
use std::error::Error;
use std::fs::File;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use dbsloc_pipeline::{checkreg, import, options::RunOptions};
use dbsloc_zurich::{Candidate, ElectrodeEvidence, Modality, Scan};
use serde_json::{json, Value};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
#[path = "cohort/fallback.rs"]
mod fallback;

const STAGES: &str = "bids,preprocess,precoreg,coreg,normalize,brainshift,reconstruct,visualize";

fn canonical(path: &Path) -> Result<PathBuf> {
    if path.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("parent traversal is not allowed".into());
    }
    Ok(std::fs::canonicalize(path)?)
}

fn subject(name: &str) -> Option<&str> {
    name.strip_prefix("SUBJ-")
        .filter(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()))
}

fn extras(scan: &Scan, primary: usize) -> Vec<usize> {
    let Some(anchor) = scan.candidates.get(primary) else {
        return Vec::new();
    };
    let mut by_type = BTreeMap::<&str, usize>::new();
    for (i, c) in scan.candidates.iter().enumerate() {
        let Some(suffix) = c.suffix.as_deref() else {
            continue;
        };
        if i == primary
            || c.modality != Modality::Mr
            || !c.eligible
            || !fallback::valid_mri(c)
            || c.explicit_postop
            || !import::PREOP_SUFFIXES.contains(&suffix)
            || c.suffix == anchor.suffix
            || (anchor.suffix.as_deref() != Some("T1w") && suffix == "T1w")
        {
            continue;
        }
        let best = by_type.entry(suffix).or_insert(i);
        let old = &scan.candidates[*best];
        let ordering = (c.session_number != anchor.session_number)
            .cmp(&(old.session_number != anchor.session_number))
            .then_with(|| old.session_number.cmp(&c.session_number))
            .then_with(|| old.quality.total_cmp(&c.quality))
            .then_with(|| c.series_uid.cmp(&old.series_uid));
        if ordering.is_lt() {
            *best = i;
        }
    }
    by_type.into_values().collect()
}

fn command(binary: &Path, dataset: &Path, id: &str) -> Command {
    let mut cmd = Command::new(binary);
    cmd.arg("run")
        .arg(dataset)
        .arg(id)
        .arg("--output-dir")
        .arg(dataset.join("derivatives").join("leaddbs"))
        .args([
            "--el-model",
            "Medtronic 3389",
            "--reg-backend",
            "auto",
            "--headless",
            "--no-checkreg",
            "--no-scene3d",
            "--fingerprint-locks",
            "--stages",
            STAGES,
            "--profile",
        ])
        .stdin(Stdio::null());
    cmd
}

fn atomic_json(path: &Path, value: &Value) -> Result<()> {
    let temporary = path.with_extension("json.tmp");
    serde_json::to_writer_pretty(File::create(&temporary)?, value)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn choice(candidate: &Candidate) -> Value {
    json!({"session":candidate.session_number,"series_uid":candidate.series_uid,
        "modality":format!("{:?}",candidate.modality),"suffix":candidate.suffix,
        "slices":candidate.slice_count,"spacing_mm":candidate.spacing_mm,
        "electrode_evidence":format!("{:?}",candidate.evidence),
        "electrode_reason":candidate.reason,"type_reason":candidate.suffix_reason})
}

fn qc(dataset: &Path, id: &str) -> Result<Value> {
    let mut opts = RunOptions::new(dataset, id).with_resolved_template();
    opts.output_dir = Some(dataset.join("derivatives").join("leaddbs"));
    let sub = opts.resolve_subject()?;
    let norm_log = checkreg::default_norm_method_log_path(&sub);
    if norm_log.is_file() {
        let record: Value = serde_json::from_reader(File::open(&norm_log)?)?;
        if let Some(template) = record
            .get("template")
            .and_then(Value::as_array)
            .and_then(|values| values.first())
            .and_then(Value::as_str)
        {
            opts.normalization_template = Some(canonical(Path::new(template))?);
        }
    }
    let mut slides =
        checkreg::coreg_slide_specs(&sub, &checkreg::default_coreg_method_log_path(&sub));
    slides.extend(checkreg::norm_slide_specs(&sub, &opts, &norm_log));
    if slides.is_empty() {
        return Ok(json!({"status":"unavailable","slides":0}));
    }
    checkreg::build_slides_with(
        &sub,
        &slides,
        &sub.subj_dir.join("batch_qc"),
        &checkreg::PngWriter::Viz,
    )
    .map_err(std::io::Error::other)?;
    Ok(
        json!({"status":"pending_manual_review","slides":slides.len(),"manifest":checkreg::manifest_path(&sub)}),
    )
}

fn process_pair(
    scan: &Scan,
    mri: usize,
    ct: usize,
    dataset: &Path,
    binary: &Path,
    id: &str,
    logs: &Path,
) -> Result<Value> {
    let cancel = AtomicBool::new(false);
    let extra = extras(scan, mri);
    let mut selected = vec![choice(&scan.candidates[mri]), choice(&scan.candidates[ct])];
    let patient = logs.join(format!("sub-{id}"));
    std::fs::create_dir(&patient)?;
    let mut audit = json!({"subject":id,"status":"preparing","selected":selected,"warnings":scan.warnings,"date_policy":"Anonymized dates advisory; MRI pre-op status requires manual review"});
    atomic_json(&patient.join("scan_choices.json"), &audit)?;
    let temp = tempfile::tempdir()?;
    let mut items = dbsloc_zurich::prepare_selection(
        scan,
        mri,
        ct,
        &temp.path().join("primary"),
        &cancel,
        |_| {},
    )?;
    let mut warnings = Vec::new();
    for (k, index) in extra.into_iter().enumerate() {
        match dbsloc_zurich::prepare_selection(
            scan,
            index,
            ct,
            &temp.path().join(format!("extra-{k}")),
            &cancel,
            |_| {},
        ) {
            Ok(pair) => {
                let mr = pair
                    .into_iter()
                    .find(|i| i.session == import::Session::Preop)
                    .ok_or("prepared extra pair has no MRI")?;
                items.push(mr);
                selected.push(choice(&scan.candidates[index]));
            }
            Err(error) => warnings.push(format!(
                "optional {:?} skipped: {error}",
                scan.candidates[index].suffix
            )),
        }
    }
    let plan = import::plan_import(dataset, id, &items)?;
    import::run_import(&plan, false, &cancel, &mut |_| {})?;
    audit["selected"] = json!(selected);
    audit["optional_conversion_warnings"] = json!(warnings);
    audit["status"] = json!("imported_pending_review");
    atomic_json(&patient.join("scan_choices.json"), &audit)?;
    drop(temp);
    let state = patient.join("status.json");
    atomic_json(
        &state,
        &json!({"subject":id,"status":"processing","modalities":items.iter().map(|i|i.suffix.as_str()).collect::<Vec<_>>()}),
    )?;
    let pipeline_json = patient.join("pipeline.json");
    let stderr = patient.join("pipeline.stderr.log");
    let mut cmd = command(binary, dataset, id);
    configure_template(
        &mut cmd,
        dataset,
        items.iter().any(|i| i.suffix == "T1w"),
        items.iter().any(|i| i.suffix == "T2w"),
    )?;
    let status = cmd
        .stdout(Stdio::from(File::create(&pipeline_json)?))
        .stderr(Stdio::from(File::create(&stderr)?))
        .status();
    let review =
        qc(dataset, id).unwrap_or_else(|e| json!({"status":"failed","error":e.to_string()}));
    let reconstruction = dataset.join("derivatives/leaddbs").join(format!(
        "sub-{id}/reconstruction/sub-{id}_desc-reconstruction.json"
    ));
    let (success, exit_code, error) = match status {
        Ok(s) => (s.success(), s.code(), None),
        Err(e) => (false, None, Some(e.to_string())),
    };
    let complete =
        success && fallback::traced(dataset, id) && review["status"] == "pending_manual_review";
    let result = json!({"subject":id,"status":if complete {"processed_pending_review"}else{"failed"},
        "pipeline_exit_code":exit_code,"launch_error":error,"reconstruction_exists":reconstruction.is_file(),
        "modalities":items.iter().map(|i|i.suffix.as_str()).collect::<Vec<_>>(),"warnings":warnings,
        "qc":review,"scan_choices":patient.join("scan_choices.json"),"pipeline_json":pipeline_json,"stderr":stderr});
    atomic_json(&state, &result)?;
    Ok(result)
}

fn configure_template(cmd: &mut Command, dataset: &Path, t1: bool, t2: bool) -> Result<()> {
    if !t1 && t2 {
        let opts = RunOptions::new(dataset, "check").with_resolved_template();
        let template = opts
            .normalization_template_t2
            .ok_or("T2-only scan needs a shipped T2 template")?;
        cmd.arg("--template")
            .arg(canonical(&template)?)
            .arg("--no-norm-multispectral");
    }
    cmd.env("DBSLOC_STOP_ON_ERROR", "1");
    Ok(())
}

fn finalize(dataset: &Path, binary: &Path, id: &str, log: &Path) -> Result<()> {
    let rows: Value = serde_json::from_reader(File::open(
        dataset
            .join("rawdata")
            .join(format!("sub-{id}/rawimages.json")),
    )?)?;
    let data = rows.to_string();
    let mut cmd = command(binary, dataset, id);
    cmd.arg("--stages").arg("reconstruct,visualize");
    // clap forbids repeated --stages, so replace the original value below.
    let mut args: Vec<_> = cmd.get_args().map(std::ffi::OsStr::to_owned).collect();
    let n = args.len();
    args.truncate(n - 2);
    if let Some(pos) = args.iter().position(|v| v == "--stages") {
        args[pos + 1] = "reconstruct,visualize".into();
    }
    let mut run = Command::new(binary);
    run.args(args).stdin(Stdio::null());
    configure_template(
        &mut run,
        dataset,
        data.contains("T1w"),
        data.contains("T2w"),
    )?;
    let status = run
        .stdout(File::create(log.join("final_pipeline.json"))?)
        .stderr(File::create(log.join("final_pipeline.stderr.log"))?)
        .status()?;
    if !status.success() || !fallback::traced(dataset, id) {
        return Err("canonical export/localization failed".into());
    }
    let review = qc(dataset, id)?;
    if review["status"] != "pending_manual_review" {
        return Err("canonical review unavailable".into());
    }
    atomic_json(&log.join("final_qc.json"), &review)?;
    verify_references(dataset, id)?;
    Ok(())
}

fn verify_references(dataset: &Path, id: &str) -> Result<()> {
    let sub = dataset
        .join("derivatives/leaddbs")
        .join(format!("sub-{id}"));
    for p in [
        sub.join("locks/checkreg_manifest.json"),
        sub.join("reconstruction/viz/scene_manifest.json"),
        fallback::reconstruction(dataset, id),
    ] {
        let v: Value = serde_json::from_reader(File::open(p)?)?;
        fn visit(v: &Value, root: &str) -> Result<()> {
            match v {
                Value::String(s) if s.starts_with(root) => {
                    if !Path::new(s).exists() {
                        return Err(format!("published reference missing: {s}").into());
                    }
                }
                Value::Array(a) => {
                    for v in a {
                        visit(v, root)?
                    }
                }
                Value::Object(o) => {
                    for v in o.values() {
                        visit(v, root)?
                    }
                }
                _ => {}
            }
            Ok(())
        }
        visit(&v, &dataset.to_string_lossy())?;
    }
    Ok(())
}

fn process(
    source: &Path,
    dataset: &Path,
    binary: &Path,
    id: &str,
    logs: &Path,
    max_attempts: usize,
) -> Result<Value> {
    let scan = dbsloc_zurich::scan_patient(source, &AtomicBool::new(false), |_| {})?;
    let pairs = fallback::pairs(&scan, max_attempts);
    if pairs.is_empty() {
        return Ok(
            json!({"subject":id,"status":"skipped","reason":"no supported MRI / metal-positive CT pair; unassigned types are not guessed","warnings":scan.warnings}),
        );
    }
    let patient = logs.join(format!("sub-{id}"));
    std::fs::create_dir(&patient)?;
    let mut attempts = Vec::new();
    let (success, errors) = fallback::attempt_until_success(&pairs, |n, (mri, ct)| {
        let attempt = patient.join(format!("attempt-{:02}", n + 1));
        std::fs::create_dir(&attempt)?;
        let staged = attempt.join("dataset");
        std::fs::create_dir(&staged)?;
        let result: Result<Value> = (|| {
            let record = process_pair(&scan, mri, ct, &staged, binary, id, &attempt)?;
            if record["status"] != "processed_pending_review" {
                return Err("pipeline or localization rejected scan pair".into());
            }
            fallback::publish(&staged, dataset, id, &attempt.join("backup"), || {
                finalize(dataset, binary, id, &attempt)
            })?;
            Ok(record)
        })();
        let mut entry = json!({"attempt":n+1,"mri":choice(&scan.candidates[mri]),"ct":choice(&scan.candidates[ct]),"attempt_directory":attempt,"date_policy":"advisory only; selected MRI requires pre-op review"});
        match &result {
            Ok(_) => entry["status"] = json!("published_pending_review"),
            Err(e) => {
                entry["status"] = json!("failed");
                entry["error"] = json!(e.to_string());
                retain_qc_and_clean(&staged, &attempt)?;
                entry["volume_artifacts_retained"] = json!(false);
            }
        }
        atomic_json(&attempt.join("outcome.json"), &entry)?;
        attempts.push(entry);
        atomic_json(&patient.join("attempts.json"), &json!(attempts))?;
        println!(
            "sub-{id} attempt {}/{}: {}",
            n + 1,
            pairs.len(),
            if result.is_ok() { "success" } else { "failed" }
        );
        result
    });
    let record = json!({"subject":id,"status":if success.is_some(){"processed_pending_review"}else{"failed"},"attempts":attempts,"errors":errors,"reconstruction_exists":fallback::reconstruction(dataset,id).exists(),"qc_manifest":dataset.join("derivatives/leaddbs").join(format!("sub-{id}/locks/checkreg_manifest.json")),"manual_review":"pending"});
    atomic_json(&patient.join("status.json"), &record)?;
    Ok(record)
}

fn retain_qc_and_clean(staged: &Path, attempt: &Path) -> Result<()> {
    if staged != attempt.join("dataset")
        || !attempt
            .file_name()
            .is_some_and(|s| s.to_string_lossy().starts_with("attempt-"))
    {
        return Err("refusing cleanup outside owned attempt dataset".into());
    }
    if staged.exists() {
        let output = attempt.join("qc_images");
        std::fs::create_dir_all(&output)?;
        fn copy(dir: &Path, to: &Path) -> Result<()> {
            for e in std::fs::read_dir(dir)? {
                let e = e?;
                let ty = e.file_type()?;
                if ty.is_symlink() {
                    return Err("symlink in owned attempt".into());
                }
                if ty.is_dir() {
                    copy(&e.path(), to)?
                } else if e.path().extension().is_some_and(|s| s == "png") {
                    std::fs::copy(e.path(), to.join(e.file_name()))?;
                }
            }
            Ok(())
        }
        copy(staged, &output)?;
        std::fs::remove_dir_all(staged)?;
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args.len() < 3 {
        return Err(
            "usage: cohort SOURCE DATASET DBSLOC_BIN [--skip ID] [--review-only ID] [--retry-failed] [--max-attempts N]".into(),
        );
    }
    let source = canonical(Path::new(&args[0]))?;
    let dataset = canonical(Path::new(&args[1]))?;
    let binary = canonical(Path::new(&args[2]))?;
    let mut skip = "000003".to_owned();
    let mut retry_failed = false;
    let mut max_attempts = 8usize;
    let mut review_only = None;
    let mut k = 3;
    while k < args.len() {
        if args[k] == "--retry-failed" {
            retry_failed = true;
            k += 1;
            continue;
        }
        let value = args
            .get(k + 1)
            .and_then(|v| v.to_str())
            .ok_or("option needs a value")?;
        if args[k] == "--max-attempts" {
            max_attempts = value.parse()?;
            if max_attempts == 0 {
                return Err("max-attempts must be positive".into());
            }
        } else if args[k] == "--skip" || args[k] == "--review-only" {
            if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
                return Err("ID must contain digits only".into());
            }
            if args[k] == "--skip" {
                skip = value.to_owned()
            } else {
                review_only = Some(value.to_owned())
            }
        } else {
            return Err("unknown option".into());
        }
        k += 2;
    }
    if let Some(id) = review_only {
        println!("{}", serde_json::to_string_pretty(&qc(&dataset, &id)?)?);
        return Ok(());
    }
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let logs = dataset
        .join("batch_logs")
        .join(format!("zurich-{stamp}-{}", std::process::id()));
    std::fs::create_dir_all(&logs)?;
    let mut patients = Vec::new();
    for entry in std::fs::read_dir(&source)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let Some(id) = entry
            .file_name()
            .to_str()
            .and_then(subject)
            .map(str::to_owned)
        else {
            continue;
        };
        patients.push((id, entry.path()));
    }
    patients.sort_by(|a, b| a.0.cmp(&b.0));
    let mut records = Vec::new();
    println!("batch log: {}", logs.display());
    for (id, path) in patients {
        let start = Instant::now();
        let exists = dataset.join("rawdata").join(format!("sub-{id}")).exists()
            || dataset
                .join("derivatives/leaddbs")
                .join(format!("sub-{id}"))
                .exists();
        let protection = fallback::protected(&dataset, &id);
        let mut record = if let Err(e) = &protection {
            json!({"subject":id,"status":"skipped","reason":format!("cannot verify existing approval protection: {e}")})
        } else if id == "000003"
            || id == skip
            || protection.unwrap_or(true)
            || (exists && !retry_failed)
        {
            json!({"subject":id,"status":"skipped","reason":if id=="000003"||id==skip {"explicitly excluded"}else{"existing subject retained"}})
        } else {
            process(&path, &dataset, &binary, &id, &logs, max_attempts)
                .unwrap_or_else(|e| json!({"subject":id,"status":"failed","error":e.to_string()}))
        };
        record["seconds"] = json!(start.elapsed().as_secs_f64());
        println!(
            "sub-{id}: {} ({:.1}s)",
            record["status"].as_str().unwrap_or("unknown"),
            start.elapsed().as_secs_f64()
        );
        records.push(record);
        atomic_json(
            &logs.join("summary.json"),
            &json!({"source":source,"dataset":dataset,"electrode_model":"Medtronic 3389","manual_review":"pending","subjects":records}),
        )?;
    }
    if records.iter().any(|r| r["status"] == "failed") {
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    pub(super) fn candidate(session: u32, suffix: &str, quality: f32) -> Candidate {
        Candidate {
            session_number: session,
            session_path: PathBuf::new(),
            series_uid: format!("{session}-{quality}"),
            modality: Modality::Mr,
            description: String::new(),
            study_date: None,
            slice_count: 100,
            spacing_mm: [1.; 3],
            evidence: ElectrodeEvidence::LikelyAbsent,
            reason: String::new(),
            eligible: true,
            suffix: Some(suffix.into()),
            suffix_reason: String::new(),
            preview: None,
            slices: Vec::new(),
            quality,
            explicit_preop: true,
            explicit_postop: false,
        }
    }
    #[test]
    fn parses_only_numeric_pseudonymous_subjects() {
        assert_eq!(subject("SUBJ-000003"), Some("000003"));
        for bad in ["Patient-name", "SUBJ-", "SUBJ-../../3", "SUBJ-3x"] {
            assert_eq!(subject(bad), None);
        }
    }
    #[test]
    fn extras_prefer_primary_session_then_oldest_and_quality() {
        let scan = Scan {
            candidates: vec![
                candidate(3, "T1w", 1.),
                candidate(5, "T2w", 100.),
                candidate(3, "T2w", 1.),
                candidate(2, "FLAIR", 100.),
                candidate(4, "FLAIR", 0.),
            ],
            ..Scan::default()
        };
        let selected = extras(&scan, 0);
        assert_eq!(selected, vec![4, 2]);
    }
    #[test]
    fn extras_exclude_unknown_postop_unsupported_and_same_type() {
        let mut rows = vec![
            candidate(3, "T1w", 1.),
            candidate(3, "T2w", 1.),
            candidate(3, "FLAIR", 1.),
            candidate(3, "PDw", 1.),
            candidate(4, "T1w", 2.),
        ];
        rows[1].explicit_postop = true;
        rows[2].explicit_postop = true;
        rows[3].eligible = false;
        assert!(extras(
            &Scan {
                candidates: rows,
                ..Scan::default()
            },
            0
        )
        .is_empty());
    }
    #[test]
    fn failed_attempt_cleanup_preserves_unrelated_directory() -> Result<()> {
        let t = tempfile::tempdir()?;
        let a = t.path().join("attempt-01");
        std::fs::create_dir(&a)?;
        let d = a.join("dataset");
        std::fs::create_dir(&d)?;
        std::fs::write(d.join("volume.nii"), "volume")?;
        std::fs::write(d.join("qc.png"), "png")?;
        let other = t.path().join("unrelated");
        std::fs::create_dir(&other)?;
        std::fs::write(other.join("keep"), "keep")?;
        assert!(retain_qc_and_clean(&other, &a).is_err());
        retain_qc_and_clean(&d, &a)?;
        assert!(!d.exists());
        assert!(a.join("qc_images/qc.png").exists());
        assert!(other.join("keep").exists());
        Ok(())
    }
    #[test]
    fn unattended_command_keeps_review_unapproved_and_multispectral_enabled() {
        let c = command(Path::new("dbsloc"), Path::new("dataset"), "000001");
        let a: Vec<_> = c
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        for flag in [
            "--headless",
            "--no-checkreg",
            "--no-scene3d",
            "--fingerprint-locks",
        ] {
            assert!(a.iter().any(|s| s == flag));
        }
        assert!(a.windows(2).any(|p| p == ["--el-model", "Medtronic 3389"]));
        assert!(a.windows(2).any(|p| p == ["--stages", STAGES]));
        assert!(!a
            .iter()
            .any(|s| s == "--overwrite-approved" || s == "--no-norm-multispectral"));
    }
}
