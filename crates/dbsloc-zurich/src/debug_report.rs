// SPDX-FileCopyrightText: 2026 Wyss Center for Bio and Neuroengineering
// SPDX-License-Identifier: GPL-3.0-only

//! 100% PII-free anonymized debug report for diagnosing Zurich scan-selection or contrast
//! mistakes across machines without sharing patient names, dates, paths, UIDs, or images.

use serde_json::{json, Value};

use crate::{Modality, Scan};

/// Build a sanitized, PII-free JSON debug report for a scanned patient folder.
///
/// Omits patient IDs, filesystem paths, study dates, series UIDs, and image pixel data.
/// Retains only relative session numbers (`CT/export-004`, `MR/export-002`), acquisition geometry,
/// CT kernel classification, artifact detector telemetry, and exact selection/rejection reasons.
pub fn build_debug_report(scan: &Scan, case_label: &str) -> Value {
    let candidates: Vec<Value> = scan
        .candidates
        .iter()
        .enumerate()
        .map(|(index, c)| {
            let folder = match c.modality {
                Modality::Ct => format!("CT/export-{:03}", c.session_number),
                Modality::Mr => format!("MR/export-{:03}", c.session_number),
            };
            json!({
                "index": index,
                "session": folder,
                "session_number": c.session_number,
                "modality": format!("{:?}", c.modality),
                "slice_count": c.slice_count,
                "spacing_mm": c.spacing_mm,
                "quality_score": c.quality,
                "eligible": c.eligible,
                "ct_kernel": format!("{:?}", c.ct_kernel),
                "orientation": c.orientation.map(|o| o.as_str()),
                "suffix": c.suffix,
                "contrast_source": format!("{:?}", c.contrast_source),
                "suffix_reason": c.suffix_reason,
                "evidence": format!("{:?}", c.evidence),
                "reason": c.reason,
                "explicit_preop": c.explicit_preop,
                "explicit_postop": c.explicit_postop,
                "telemetry": {
                    "sampled_slices": c.telemetry.sampled_slices,
                    "positive_slices": c.telemetry.positive_slices,
                    "intracranial_metal_counts": c.telemetry.intracranial_metal_counts,
                    "max_hu": c.telemetry.max_hu,
                    "used_intracranial_mask": c.telemetry.used_intracranial_mask,
                    "mr_void_slices": c.telemetry.mr_void_slices,
                    "mr_void_counts": c.telemetry.mr_void_counts,
                },
            })
        })
        .collect();

    json!({
        "schema": "dbsloc-zurich-debug/1",
        "case": case_label,
        "selected": {
            "postop_ct": scan.postop_ct,
            "preop_mri_primary": scan.preop_mri,
            "preop_mris": scan.preop_mris,
        },
        "warnings": scan.warnings,
        "candidate_count": candidates.len(),
        "candidates": candidates,
        "expected_override_notes": "Fill in what should have been selected if incorrect, then share with dev."
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArtifactTelemetry, Candidate, ContrastSource, CtKernel, ElectrodeEvidence};
    use std::path::PathBuf;

    #[test]
    fn debug_report_excludes_paths_dates_and_uids() {
        let scan = Scan {
            candidates: vec![Candidate {
                session_number: 4,
                session_path: PathBuf::from(r"C:\Hospital\JohnDoe_19700101\CT\export-004"),
                series_uid: "1.2.840.113619.999999.SECRET".into(),
                modality: Modality::Ct,
                description: "PATIENT_JOHN_DOE_BRAIN_SOFT".into(),
                study_date: Some("20250115".into()),
                slice_count: 300,
                spacing_mm: [0.5, 0.5, 0.5],
                evidence: ElectrodeEvidence::Detected,
                reason: "Detected".into(),
                eligible: true,
                suffix: Some("CT".into()),
                suffix_reason: "CT".into(),
                preview: None,
                slices: Vec::new(),
                quality: 2.6,
                explicit_preop: false,
                explicit_postop: false,
                ct_kernel: CtKernel::SoftTissue,
                orientation: Some(crate::AcqOrientation::Ax),
                contrast_source: ContrastSource::DicomText,
                telemetry: ArtifactTelemetry::default(),
            }],
            preop_mri: None,
            preop_mris: Vec::new(),
            postop_ct: Some(0),
            warnings: Vec::new(),
        };
        let text = serde_json::to_string_pretty(&build_debug_report(&scan, "CASE-01"))
            .expect("serialize debug report");
        assert!(!text.contains("JohnDoe"));
        assert!(!text.contains("JOHN_DOE"));
        assert!(!text.contains("19700101"));
        assert!(!text.contains("20250115"));
        assert!(!text.contains("SECRET"));
        assert!(text.contains("CT/export-004"));
        assert!(text.contains("SoftTissue"));
    }
}
