// SPDX-FileCopyrightText: 2026 Wyss Center for Bio and Neuroengineering
// SPDX-License-Identifier: GPL-3.0-only

//! Zurich-specific triage; final BIDS writes belong to `dbsloc_pipeline::import`.

mod prepare;
mod scan;

use std::path::PathBuf;

pub use prepare::prepare_selection;
pub use scan::scan_patient;

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
