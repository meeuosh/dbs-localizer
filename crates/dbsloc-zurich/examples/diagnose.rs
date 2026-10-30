//! Read-only, local diagnostic inventory for Zurich scan-selection decisions.
use std::path::{Component, Path};
use std::sync::atomic::AtomicBool;

use serde_json::json;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let source = std::env::args_os()
        .nth(1)
        .ok_or("usage: diagnose PATIENT")?;
    let source = Path::new(&source);
    if source.components().any(|c| c == Component::ParentDir) {
        return Err("parent traversal is not allowed".into());
    }
    let source = source.canonicalize()?;
    let scan = dbsloc_zurich::scan_patient(&source, &AtomicBool::new(false), |_| {})?;
    let candidates: Vec<_> = scan.candidates.iter().enumerate().map(|(index, c)| {
        let positions: Vec<_> = c.slices.iter().filter_map(|s|s.position).collect();
        let steps: Vec<_> = c.slices.windows(2).filter_map(|w| {
            let (a,b)=(w[0].position?,w[1].position?);
            Some(((a[0]-b[0]).powi(2)+(a[1]-b[1]).powi(2)+(a[2]-b[2]).powi(2)).sqrt())
        }).collect();
        json!({"index":index,"session":c.session_number,"session_path":c.session_path,
            "modality":format!("{:?}",c.modality),"suffix":c.suffix,"type_reason":c.suffix_reason,
            "evidence":format!("{:?}",c.evidence),"reason":c.reason,"eligible":c.eligible,
            "slice_count":c.slice_count,"spacing_mm":c.spacing_mm,"study_date":c.study_date,
            "series_uid":c.series_uid,"quality":c.quality,"explicit_preop":c.explicit_preop,
            "explicit_postop":c.explicit_postop,"preview_decoded":c.preview.is_some(),
            "first_slice":c.slices.first().map(|s|json!({"path":s.path,"rows":s.rows,"columns":s.columns,
                "frames":s.frames,"orientation":s.orientation,"position":s.position,"spacing":s.spacing,"thickness":s.thickness})),
            "geometry":{"positions":positions.len(),"distinct_positions":positions.iter().map(|p|format!("{p:?}")).collect::<std::collections::HashSet<_>>().len(),
                "step_min":steps.iter().copied().reduce(f64::min),"step_max":steps.iter().copied().reduce(f64::max)}})
    }).collect();
    serde_json::to_writer_pretty(
        std::io::stdout().lock(),
        &json!({"preop_mri":scan.preop_mri,
        "postop_ct":scan.postop_ct,"warnings":scan.warnings,"candidates":candidates}),
    )?;
    Ok(())
}
