// SPDX-FileCopyrightText: 2026 Wyss Center for Bio and Neuroengineering
// SPDX-License-Identifier: GPL-3.0-only

//! Zurich-specific triage; final BIDS writes belong to `dbsloc_pipeline::import`.

mod debug_report;
mod fallback;
mod prepare;
mod scan;

use std::path::PathBuf;

pub use debug_report::build_debug_report;
pub use prepare::{prepare_selection, prepare_study, PreparedStudyItem};
pub use scan::scan_patient;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CtKernel {
    SoftTissue,
    #[default]
    Unknown,
    Bone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AcqOrientation {
    Iso,
    Ax,
    Cor,
    Sag,
}

impl AcqOrientation {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Iso => "iso",
            Self::Ax => "ax",
            Self::Cor => "cor",
            Self::Sag => "sag",
        }
    }

    pub fn to_import_acq(self) -> dbsloc_pipeline::import::Acq {
        match self {
            Self::Iso => dbsloc_pipeline::import::Acq::Iso,
            Self::Ax => dbsloc_pipeline::import::Acq::Ax,
            Self::Cor => dbsloc_pipeline::import::Acq::Cor,
            Self::Sag => dbsloc_pipeline::import::Acq::Sag,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ContrastSource {
    DicomText,
    DicomPhysics,
    SidecarPhysics,
    ImageIntensity,
    Manual,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, Default)]
pub struct ArtifactTelemetry {
    pub sampled_slices: usize,
    pub positive_slices: usize,
    pub intracranial_metal_counts: Vec<usize>,
    pub max_hu: f32,
    pub used_intracranial_mask: bool,
    pub mr_void_slices: usize,
    pub mr_void_counts: Vec<usize>,
}

#[derive(Clone, Debug, Default)]
pub struct IntensityStats {
    pub brain_median: f32,
    pub p90_ratio: f32,
    pub p99_ratio: f32,
    pub core_bright_ratio: f32,
    pub core_dark_ratio: f32,
    pub rim_to_median: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modality {
    Ct,
    Mr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ElectrodeEvidence {
    Detected,
    LikelyAbsent,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct Preview {
    pub width: usize,
    pub height: usize,
    pub pixels: Vec<f32>,
}

#[derive(Clone, Debug)]
pub struct Candidate {
    pub session_number: u32,
    pub session_path: PathBuf,
    pub series_uid: String,
    pub modality: Modality,
    pub description: String,
    pub study_date: Option<String>,
    pub slice_count: usize,
    pub spacing_mm: [f32; 3],
    pub evidence: ElectrodeEvidence,
    pub reason: String,
    /// Supported volume; human override is allowed independently of automatic evidence.
    pub eligible: bool,
    pub suffix: Option<String>,
    pub suffix_reason: String,
    pub preview: Option<Preview>,
    pub slices: Vec<Slice>,
    pub quality: f32,
    pub explicit_preop: bool,
    pub explicit_postop: bool,
    pub ct_kernel: CtKernel,
    pub orientation: Option<AcqOrientation>,
    pub contrast_source: ContrastSource,
    pub telemetry: ArtifactTelemetry,
}

#[derive(Clone, Debug)]
pub struct Slice {
    pub path: PathBuf,
    pub rows: usize,
    pub columns: usize,
    pub instance: i32,
    pub position: Option<[f64; 3]>,
    pub orientation: Option<[f64; 6]>,
    pub spacing: [f32; 2],
    pub thickness: f32,
    pub frames: u32,
}

#[derive(Clone, Debug, Default)]
pub struct Scan {
    pub candidates: Vec<Candidate>,
    pub preop_mri: Option<usize>,
    /// All eligible anatomical MR series from the chosen most-recent clean MR study
    /// (primary T1w first when available).
    pub preop_mris: Vec<usize>,
    pub postop_ct: Option<usize>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    pub label: String,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("operation cancelled")]
    Cancelled,
    #[error("parent traversal is not allowed: {0}")]
    Traversal(PathBuf),
    #[error("{action}: {source}")]
    Io {
        action: &'static str,
        #[source]
        source: std::io::Error,
    },
    #[error("no supported CT/MR numbered sessions found")]
    NoSessions,
    #[error("no readable CT/MR image series found")]
    NoSeries,
    #[error("invalid selection: {0}")]
    Selection(&'static str),
    #[error("output directory already exists")]
    OutputExists,
    #[error("selected {target} produced {count} volumes; exactly one is required")]
    VolumeCount { target: &'static str, count: usize },
    #[error("DICOM conversion failed: {0}")]
    Conversion(#[from] dbsloc_pipeline::import::ImportError),
    #[error("unsupported or malformed NIfTI header: {0}")]
    Nifti(&'static str),
    #[error("malformed converter sidecar: {0}")]
    Sidecar(#[from] serde_json::Error),
}

fn io(action: &'static str, source: std::io::Error) -> Error {
    Error::Io { action, source }
}

fn check_cancel(cancel: &std::sync::atomic::AtomicBool) -> Result<(), Error> {
    if cancel.load(std::sync::atomic::Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

fn reject_traversal(path: &std::path::Path) -> Result<(), Error> {
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        Err(Error::Traversal(path.to_path_buf()))
    } else {
        Ok(())
    }
}

/// Canonicalize `path` and, on Windows, strip the `\\?\` verbatim prefix so external tools
/// such as `dcm2niix.exe` and `dbsloc.exe` receive standard drive or UNC paths.
pub fn canonical_path(path: &std::path::Path) -> std::io::Result<PathBuf> {
    let canonical = std::fs::canonicalize(path)?;
    #[cfg(windows)]
    {
        if let Some(stripped) = canonical.to_str().and_then(strip_windows_verbatim_str) {
            return Ok(PathBuf::from(stripped));
        }
    }
    Ok(canonical)
}

#[cfg_attr(not(any(windows, test)), allow(dead_code))]
fn strip_windows_verbatim_str(raw: &str) -> Option<String> {
    if let Some(unc) = raw.strip_prefix(r"\\?\UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    let rest = raw.strip_prefix(r"\\?\")?;
    let bytes = rest.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return Some(rest.to_owned());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::strip_windows_verbatim_str;

    #[test]
    fn strips_windows_verbatim_drive_and_unc_prefixes() {
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\C:\Users\milosz\Temp\stage_0").as_deref(),
            Some(r"C:\Users\milosz\Temp\stage_0")
        );
        assert_eq!(
            strip_windows_verbatim_str(r"\\?\UNC\server\share\zurich\pt01").as_deref(),
            Some(r"\\server\share\zurich\pt01")
        );
        assert_eq!(strip_windows_verbatim_str(r"C:\plain\path"), None);
        assert_eq!(strip_windows_verbatim_str("/tmp/linux/path"), None);
        assert_eq!(strip_windows_verbatim_str(r"\\?\Volume{1234}\foo"), None);
    }
}
