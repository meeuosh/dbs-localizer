// SPDX-FileCopyrightText: 2026 Wyss Center for Bio and Neuroengineering
// SPDX-License-Identifier: GPL-3.0-only

use super::*;
use std::collections::HashSet;

pub fn valid_mri(c: &Candidate) -> bool {
    c.eligible
        && c.modality == Modality::Mr
        && !c.explicit_postop
        && c.suffix
            .as_deref()
            .is_some_and(|s| import::PREOP_SUFFIXES.contains(&s))
}

pub fn pairs(scan: &Scan, limit: usize) -> Vec<(usize, usize)> {
    let rank = |a: &usize, b: &usize| {
        let (a, b) = (&scan.candidates[*a], &scan.candidates[*b]);
        b.session_number
            .cmp(&a.session_number)
            .then_with(|| {
                (b.suffix.as_deref() == Some("T1w")).cmp(&(a.suffix.as_deref() == Some("T1w")))
            })
            .then_with(|| b.quality.total_cmp(&a.quality))
            .then_with(|| a.series_uid.cmp(&b.series_uid))
    };
    let mut mr: Vec<_> = scan
        .candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| valid_mri(c))
        .map(|(i, _)| i)
        .collect();
    let mut ct: Vec<_> = scan
        .candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            c.eligible && c.modality == Modality::Ct && c.evidence == ElectrodeEvidence::Detected
        })
        .map(|(i, _)| i)
        .collect();
    mr.sort_by(|a, b| {
        (scan.candidates[*b].suffix.as_deref() == Some("T1w"))
            .cmp(&(scan.candidates[*a].suffix.as_deref() == Some("T1w")))
            .then_with(|| rank(a, b))
    });
    if let Some(initial) = scan.preop_mri.filter(|i| mr.contains(i)) {
        mr.retain(|i| *i != initial);
        mr.insert(0, initial);
    }
    ct.sort_by(rank);
    // A representative of every session precedes repeated kernels from one session.
    let mut sessions = HashSet::new();
    let (mut representatives, mut rest) = (Vec::new(), Vec::new());
    for i in ct {
        if sessions.insert(scan.candidates[i].session_path.clone()) {
            representatives.push(i)
        } else {
            rest.push(i)
        }
    }
    representatives.extend(rest);
    let ct = representatives;
    let mut seen = HashSet::new();
    let mut result = Vec::new();
    for diagonal in 0..mr.len().saturating_add(ct.len()) {
        for mi in 0..=diagonal.min(mr.len().saturating_sub(1)) {
            let ci = diagonal - mi;
            let (Some(&m), Some(&c)) = (mr.get(mi), ct.get(ci)) else {
                continue;
            };
            let a = &scan.candidates[m];
            let b = &scan.candidates[c];
            if seen.insert((
                a.session_path.clone(),
                a.series_uid.clone(),
                b.session_path.clone(),
                b.series_uid.clone(),
            )) {
                result.push((m, c));
            }
            if result.len() == limit {
                return result;
            }
        }
    }
    result
}

pub fn reconstruction(root: &Path, id: &str) -> PathBuf {
    root.join("derivatives/leaddbs").join(format!(
        "sub-{id}/reconstruction/sub-{id}_desc-reconstruction.json"
    ))
}

pub fn traced(root: &Path, id: &str) -> bool {
    let Ok(f) = File::open(reconstruction(root, id)) else {
        return false;
    };
    let Ok(v) = serde_json::from_reader::<_, Value>(f) else {
        return false;
    };
    let s = &v["meta"]["reconstruct"]["status"];
    s["1"] == "traced" && s["2"] == "traced"
}

fn approved(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.iter().any(|(k, v)| {
            ((k == "approval" || k == "approved") && positive(v))
                || (k == "manual_correction" && v.as_object().is_some_and(|x| !x.is_empty()))
                || (k == "review"
                    && matches!(v.as_str(), Some("accepted" | "corrected" | "approved")))
                || approved(v)
        }),
        Value::Array(a) => a.iter().any(approved),
        _ => false,
    }
}
fn positive(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|x| x > 0.),
        Value::Object(m) => m.values().any(positive),
        Value::Array(a) => a.iter().any(positive),
        _ => false,
    }
}

pub fn protected(root: &Path, id: &str) -> Result<bool> {
    if id == "000003" || reconstruction(root, id).exists() {
        return Ok(true);
    }
    let dir = root.join("derivatives/leaddbs").join(format!("sub-{id}"));
    for p in files(&dir)? {
        if p.starts_with(dir.join("reconstruction")) && p.extension().is_some_and(|e| e == "mat") {
            return Ok(true);
        }
        if p.extension().is_some_and(|e| e == "json") {
            let v: Value = serde_json::from_reader(File::open(&p)?)?;
            if approved(&v) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn files(dir: &Path) -> Result<Vec<PathBuf>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    if std::fs::symlink_metadata(dir)?.file_type().is_symlink() {
        return Err("symlinked subject directory is not supported".into());
    }
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let e = entry?;
        let t = e.file_type()?;
        if t.is_symlink() {
            return Err("symlinked subject output is not supported".into());
        }
        if t.is_dir() {
            out.extend(files(&e.path())?)
        } else if t.is_file() {
            out.push(e.path())
        }
    }
    Ok(out)
}
fn replace(v: &mut Value, old: &str, new: &str) {
    match v {
        Value::String(s) => *s = s.replace(old, new),
        Value::Array(a) => {
            for v in a {
                replace(v, old, new)
            }
        }
        Value::Object(m) => {
            for v in m.values_mut() {
                replace(v, old, new)
            }
        }
        _ => {}
    }
}
fn rewrite(dir: &Path, old: &str, new: &str) -> Result<()> {
    for p in files(dir)? {
        if p.extension().is_some_and(|e| e == "json") {
            let mut v: Value = serde_json::from_reader(File::open(&p)?)?;
            replace(&mut v, old, new);
            atomic_json(&p, &v)?;
        } else if p
            .extension()
            .is_some_and(|e| matches!(e.to_str(), Some("txt" | "log" | "mat")))
        {
            let Ok(s) = std::fs::read_to_string(&p) else {
                continue;
            };
            if s.contains(old) {
                std::fs::write(&p, s.replace(old, new))?
            }
        }
    }
    Ok(())
}

pub fn publish(
    staged: &Path,
    dataset: &Path,
    id: &str,
    backup: &Path,
    finish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    if protected(dataset, id)? {
        return Err("existing reconstruction or approval is protected".into());
    }
    std::fs::create_dir(backup)?;
    let old = staged.to_string_lossy();
    let new = dataset.to_string_lossy();
    let roots = [
        PathBuf::from(format!("rawdata/sub-{id}")),
        PathBuf::from(format!("derivatives/leaddbs/sub-{id}")),
    ];
    let mut moved: Vec<(PathBuf, bool)> = Vec::new();
    let result = (|| {
        for (i, rel) in roots.iter().enumerate() {
            let src = staged.join(rel);
            let dst = dataset.join(rel);
            let saved = backup.join(i.to_string());
            std::fs::create_dir_all(dst.parent().ok_or("subject has no parent")?)?;
            let had = dst.exists();
            if had {
                std::fs::rename(&dst, &saved)?
            }
            moved.push((rel.clone(), had));
            std::fs::rename(&src, &dst)?;
            rewrite(&dst, &old, &new)?;
        }
        finish()
    })();
    if result.is_err() {
        for (i, (rel, had)) in moved.iter().enumerate().rev() {
            let dst = dataset.join(rel);
            let src = staged.join(rel);
            if dst.exists() {
                std::fs::create_dir_all(src.parent().ok_or("attempt has no parent")?)?;
                std::fs::rename(&dst, &src)?;
                rewrite(&src, &new, &old)?;
            }
            if *had {
                std::fs::rename(backup.join(i.to_string()), &dst)?
            }
        }
    }
    result
}

pub fn attempt_until_success<T>(
    pairs: &[(usize, usize)],
    mut run: impl FnMut(usize, (usize, usize)) -> Result<T>,
) -> (Option<T>, Vec<String>) {
    let mut errors = Vec::new();
    for (n, &pair) in pairs.iter().enumerate() {
        match run(n, pair) {
            Ok(v) => return (Some(v), errors),
            Err(e) => errors.push(e.to_string()),
        }
    }
    (None, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn alternatives_and_exhaustion() {
        let mut visited = Vec::new();
        let (v, e) = attempt_until_success(&[(0, 1), (0, 2), (1, 2)], |n, p| {
            visited.push(p);
            if n == 0 {
                Err("bad scan".into())
            } else {
                Ok(p)
            }
        });
        assert_eq!(v, Some((0, 2)));
        assert_eq!(e.len(), 1);
        assert_eq!(visited.len(), 2);
        let (v, e) = attempt_until_success::<()>(&[(0, 1), (0, 2)], |_, _| Err("failure".into()));
        assert!(v.is_none());
        assert_eq!(e.len(), 2);
    }
    #[test]
    fn publish_rolls_back_and_rewrites() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let dst = temp.path().join("final");
        let src = temp.path().join("attempt");
        for root in [&dst, &src] {
            std::fs::create_dir_all(root.join("rawdata/sub-1"))?;
            std::fs::create_dir_all(root.join("derivatives/leaddbs/sub-1"))?;
        }
        std::fs::write(dst.join("rawdata/sub-1/old"), "original")?;
        atomic_json(
            &src.join("rawdata/sub-1/path.json"),
            &json!({"path":src.join("rawdata/sub-1/data")}),
        )?;
        assert!(
            publish(&src, &dst, "1", &temp.path().join("backup"), || Err(
                "export failed".into()
            ))
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(dst.join("rawdata/sub-1/old"))?,
            "original"
        );
        publish(&src, &dst, "1", &temp.path().join("backup2"), || Ok(()))?;
        let v: Value = serde_json::from_reader(File::open(dst.join("rawdata/sub-1/path.json"))?)?;
        assert_eq!(v["path"], json!(dst.join("rawdata/sub-1/data")));
        Ok(())
    }
    #[test]
    fn registration_mat_is_not_manual_reconstruction() -> Result<()> {
        let t = tempfile::tempdir()?;
        let d = t.path().join("derivatives/leaddbs/sub-1");
        std::fs::create_dir_all(d.join("coregistration/transformations"))?;
        std::fs::write(
            d.join("coregistration/transformations/rigid.mat"),
            "transform",
        )?;
        assert!(!protected(t.path(), "1")?);
        std::fs::create_dir_all(d.join("reconstruction"))?;
        std::fs::write(
            d.join("reconstruction/sub-1_desc-reconstruction.mat"),
            "saved",
        )?;
        assert!(protected(t.path(), "1")?);
        Ok(())
    }
    #[test]
    fn candidate_schedule_varies_mri_and_deduplicates() {
        let mut m = super::super::tests::candidate(3, "T1w", 2.);
        m.session_path = "mri".into();
        let mut m2 = m.clone();
        m2.series_uid = "other".into();
        let mut c = m.clone();
        c.modality = Modality::Ct;
        c.evidence = ElectrodeEvidence::Detected;
        c.suffix = Some("CT".into());
        c.session_path = "ct3".into();
        let mut c2 = c.clone();
        c2.session_number = 2;
        c2.session_path = "ct2".into();
        c2.series_uid = "ct2".into();
        let scan = Scan {
            candidates: vec![m, m2, c.clone(), c2, c],
            ..Scan::default()
        };
        let p = pairs(&scan, 8);
        assert_eq!(p.len(), 4);
        assert_eq!(p[0], (0, 2));
        assert_eq!(p[1], (0, 3));
        assert_eq!(p[2], (1, 2));
        assert_eq!(pairs(&scan, 2).len(), 2);
    }
    #[test]
    fn newer_t1_alternate_precedes_older_pd() {
        let mut initial = super::super::tests::candidate(3, "T1w", 1.);
        initial.session_path = "m3".into();
        let mut alternate = super::super::tests::candidate(1, "T1w", 10.);
        alternate.session_path = "m1".into();
        let mut pd = super::super::tests::candidate(3, "PDw", 100.);
        pd.session_path = "m3".into();
        let mut ct = super::super::tests::candidate(3, "CT", 1.);
        ct.modality = Modality::Ct;
        ct.evidence = ElectrodeEvidence::Detected;
        ct.session_path = "ct".into();
        let scan = Scan {
            candidates: vec![initial, alternate, pd, ct],
            preop_mri: Some(0),
            ..Scan::default()
        };
        assert_eq!(pairs(&scan, 8), vec![(0, 3), (1, 3), (2, 3)]);
    }
    #[cfg(unix)]
    #[test]
    fn fake_cli_first_failure_then_valid_reconstruction() -> Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir()?;
        let script = t.path().join("fake-cli");
        std::fs::write(&script,"#!/bin/sh\nif [ \"$1\" = 0 ]; then exit 1; fi\nmkdir -p \"$2/derivatives/leaddbs/sub-1/reconstruction\"\nprintf '%s' '{\"meta\":{\"reconstruct\":{\"status\":{\"1\":\"traced\",\"2\":\"traced\"}}}}' > \"$2/derivatives/leaddbs/sub-1/reconstruction/sub-1_desc-reconstruction.json\"\n")?;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))?;
        let mut calls = 0;
        let (success, errors) = attempt_until_success(&[(0, 1), (0, 2), (1, 2)], |n, p| {
            calls += 1;
            let status = Command::new(&script)
                .arg(n.to_string())
                .arg(t.path())
                .status()?;
            if status.success() && traced(t.path(), "1") {
                Ok(p)
            } else {
                Err("fake pipeline failed".into())
            }
        });
        assert_eq!(success, Some((0, 2)));
        assert_eq!(calls, 2);
        assert_eq!(errors.len(), 1);
        Ok(())
    }
    #[test]
    fn completed_subject_never_replaced() -> Result<()> {
        let t = tempfile::tempdir()?;
        let p = reconstruction(t.path(), "1");
        std::fs::create_dir_all(p.parent().ok_or("no parent")?)?;
        std::fs::write(&p, "existing")?;
        assert!(protected(t.path(), "1")?);
        assert!(publish(
            &t.path().join("missing"),
            t.path(),
            "1",
            &t.path().join("backup"),
            || Ok(())
        )
        .is_err());
        assert_eq!(std::fs::read_to_string(p)?, "existing");
        Ok(())
    }
}
