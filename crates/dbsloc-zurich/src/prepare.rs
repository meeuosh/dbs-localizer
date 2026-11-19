// SPDX-FileCopyrightText: 2026 Wyss Center for Bio and Neuroengineering
// SPDX-License-Identifier: GPL-3.0-only

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use dbsloc_pipeline::import::{self, ImportItem, Session};
use flate2::{read::GzDecoder, write::GzEncoder, Compression};

use crate::{check_cancel, io, reject_traversal, Candidate, Error, Modality, Progress, Scan};

struct Cleanup(Option<PathBuf>);
impl Drop for Cleanup {
    fn drop(&mut self) {
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

fn selection(scan: &Scan, mri: usize, ct: usize) -> Result<[&Candidate; 2], Error> {
    if mri == ct {
        return Err(Error::Selection("MRI and CT must be different series"));
    }
    let m = scan
        .candidates
        .get(mri)
        .ok_or(Error::Selection("MRI index out of range"))?;
    let c = scan
        .candidates
        .get(ct)
        .ok_or(Error::Selection("CT index out of range"))?;
    if m.modality != Modality::Mr || c.modality != Modality::Ct {
        return Err(Error::Selection("one MRI and one CT are required"));
    }
    if !m.eligible || !c.eligible || m.slices.is_empty() || c.slices.is_empty() {
        return Err(Error::Selection("choose supported non-scout image volumes"));
    }
    let suffix = m
        .suffix
        .as_deref()
        .ok_or(Error::Selection("MRI anatomical type must be assigned"))?;
    if !import::PREOP_SUFFIXES.contains(&suffix) {
        return Err(Error::Selection("unsupported pre-op MRI anatomical type"));
    }
    Ok([m, c])
}

fn scrub_sidecar(source: Option<&Path>, dest: &Path, modality: &str) -> Result<(), Error> {
    let value = match source {
        Some(p) => serde_json::from_reader(BufReader::new(
            File::open(p).map_err(|e| io("open converter sidecar", e))?,
        ))
        .unwrap_or(serde_json::Value::Null),
        None => serde_json::Value::Null,
    };
    let mut safe = serde_json::Map::new();
    for key in [
        "MagneticFieldStrength",
        "EchoTime",
        "RepetitionTime",
        "InversionTime",
        "FlipAngle",
        "SliceThickness",
        "SpacingBetweenSlices",
        "PixelBandwidth",
        "EffectiveEchoSpacing",
        "TotalReadoutTime",
        "EchoTrainLength",
        "MultibandAccelerationFactor",
        "ParallelReductionFactorInPlane",
        "AcquisitionMatrixPE",
        "ReconMatrixPE",
        "ImagingFrequency",
        "ImageOrientationPatientDICOM",
        "PixelSpacing",
    ] {
        if let Some(v) = value.get(key) {
            let numeric = v.is_number()
                || v.as_array()
                    .is_some_and(|a| a.iter().all(serde_json::Value::is_number));
            if numeric {
                safe.insert(key.into(), v.clone());
            }
        }
    }
    safe.insert(
        "Modality".into(),
        serde_json::Value::String(modality.into()),
    );
    let f = File::create(dest).map_err(|e| io("create sanitized sidecar", e))?;
    serde_json::to_writer_pretty(BufWriter::new(f), &safe)?;
    Ok(())
}

fn scrub_nifti(source: &Path, dest: &Path, cancel: &AtomicBool) -> Result<(), Error> {
    let file = File::open(source).map_err(|e| io("open converted NIfTI", e))?;
    let mut reader: Box<dyn Read> = if source.extension().is_some_and(|s| s == "gz") {
        Box::new(GzDecoder::new(BufReader::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };
    let mut h = [0u8; 352];
    reader
        .read_exact(&mut h)
        .map_err(|e| io("read NIfTI header", e))?;
    let little = if i32::from_le_bytes([h[0], h[1], h[2], h[3]]) == 348 {
        true
    } else if i32::from_be_bytes([h[0], h[1], h[2], h[3]]) == 348 {
        false
    } else {
        return Err(Error::Nifti(
            "only NIfTI-1 single-file images are supported",
        ));
    };
    if &h[344..348] != b"n+1\0" {
        return Err(Error::Nifti("single-file magic missing"));
    }
    let offset_bytes = [h[108], h[109], h[110], h[111]];
    let offset = if little {
        f32::from_le_bytes(offset_bytes)
    } else {
        f32::from_be_bytes(offset_bytes)
    };
    if !offset.is_finite() || !(352.0..=16_777_216.0).contains(&offset) || offset.fract() != 0.0 {
        return Err(Error::Nifti("invalid voxel offset"));
    }
    for range in [4..32, 148..252, 328..344, 348..352] {
        h[range].fill(0);
    }
    h[108..112].copy_from_slice(&if little {
        352f32.to_le_bytes()
    } else {
        352f32.to_be_bytes()
    });
    let mut remaining = offset as u64 - 352;
    let mut buffer = [0u8; 65536];
    while remaining > 0 {
        check_cancel(cancel)?;
        let take = remaining.min(buffer.len() as u64) as usize;
        reader
            .read_exact(&mut buffer[..take])
            .map_err(|e| io("skip NIfTI extensions", e))?;
        remaining -= take as u64;
    }
    let dest = File::create(dest).map_err(|e| io("create sanitized NIfTI", e))?;
    let mut writer = GzEncoder::new(BufWriter::new(dest), Compression::new(3));
    writer
        .write_all(&h)
        .map_err(|e| io("write sanitized header", e))?;
    loop {
        check_cancel(cancel)?;
        let n = reader
            .read(&mut buffer)
            .map_err(|e| io("read NIfTI voxels", e))?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&buffer[..n])
            .map_err(|e| io("write NIfTI voxels", e))?;
    }
    writer
        .finish()
        .map_err(|e| io("finish sanitized NIfTI", e))?
        .flush()
        .map_err(|e| io("flush sanitized NIfTI", e))?;
    Ok(())
}

/// Convert a reviewed pair into sanitized temporary NIfTI items, without writing a dataset.
pub fn prepare_selection(
    scan: &Scan,
    mri: usize,
    ct: usize,
    output: &Path,
    cancel: &AtomicBool,
    progress: impl Fn(Progress) + Sync,
) -> Result<Vec<ImportItem>, Error> {
    check_cancel(cancel)?;
    reject_traversal(output)?;
    let selected = selection(scan, mri, ct)?;
    if output.exists() {
        return Err(Error::OutputExists);
    }
    let parent = output
        .parent()
        .ok_or(Error::Selection("temporary output needs a parent"))?;
    let parent = std::fs::canonicalize(parent).map_err(|e| io("open temporary parent", e))?;
    let name = output
        .file_name()
        .ok_or(Error::Selection("temporary output needs a directory name"))?;
    let output = parent.join(name);
    std::fs::create_dir(&output).map_err(|e| io("create preparation directory", e))?;
    let mut cleanup = Cleanup(Some(output.clone()));
    let total = selected.iter().map(|s| s.slices.len()).sum::<usize>() + 4;
    let mut done = 0;
    let mut items = Vec::with_capacity(2);
    for (i, candidate) in selected.into_iter().enumerate() {
        check_cancel(cancel)?;
        let label = if i == 0 { "MRI" } else { "CT" };
        let stage = output.join(format!("stage_{i}"));
        let converted = output.join(format!("converted_{i}"));
        std::fs::create_dir(&stage)
            .map_err(|e| io("create selected-slice staging directory", e))?;
        let session = std::fs::canonicalize(&candidate.session_path)
            .map_err(|e| io("open selected session", e))?;
        for (n, slice) in candidate.slices.iter().enumerate() {
            check_cancel(cancel)?;
            reject_traversal(&slice.path)?;
            let meta = std::fs::symlink_metadata(&slice.path)
                .map_err(|e| io("inspect selected DICOM", e))?;
            if meta.file_type().is_symlink() {
                return Err(Error::Selection("symlinked DICOM slices are not supported"));
            }
            let source =
                std::fs::canonicalize(&slice.path).map_err(|e| io("open selected DICOM", e))?;
            if !source.starts_with(&session) {
                return Err(Error::Selection(
                    "DICOM slice lies outside selected session",
                ));
            }
            std::fs::copy(&source, stage.join(format!("slice_{n:06}.dcm")))
                .map_err(|e| io("stage selected DICOM", e))?;
            done += 1;
            if n % 32 == 0 || n + 1 == candidate.slices.len() {
                progress(Progress {
                    done,
                    total,
                    label: format!("Preparing selected {label}"),
                });
            }
        }
        progress(Progress {
            done,
            total,
            label: format!("Converting selected {label}"),
        });
        check_cancel(cancel)?;
        let mut volumes =
            import::convert_dicom_with(&stage, &converted, cancel, &import::default_lookup_table())
                .map_err(|e| {
                    if e.code == import::E_IMPORT_CANCELLED {
                        Error::Cancelled
                    } else {
                        Error::Conversion(e)
                    }
                })?;
        done += 1;
        if volumes.len() != 1 {
            return Err(Error::VolumeCount {
                target: label,
                count: volumes.len(),
            });
        }
        let volume = volumes.remove(0);
        let source = output.join(format!("selected_{i}.nii.gz"));
        let sidecar = output.join(format!("selected_{i}.json"));
        scrub_nifti(&volume.nifti, &source, cancel)?;
        scrub_sidecar(
            volume.sidecar.as_deref(),
            &sidecar,
            if i == 0 { "MR" } else { "CT" },
        )?;
        import::nifti_summary(&source)?;
        let acq = if i == 0 { volume.suggested.acq } else { None };
        items.push(ImportItem {
            source,
            sidecar: Some(sidecar),
            session: if i == 0 {
                Session::Preop
            } else {
                Session::Postop
            },
            suffix: if i == 0 {
                candidate
                    .suffix
                    .clone()
                    .ok_or(Error::Selection("missing MRI type"))?
            } else {
                "CT".into()
            },
            acq,
        });
        std::fs::remove_dir_all(&stage)
            .map_err(|e| io("remove selected-slice staging directory", e))?;
        std::fs::remove_dir_all(&converted).map_err(|e| io("remove converter metadata", e))?;
        done += 1;
        progress(Progress {
            done,
            total,
            label: format!("Prepared selected {label}"),
        });
    }
    check_cancel(cancel)?;
    cleanup.0 = None;
    Ok(items)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_after_staging_cleans_output_and_preserves_source(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let source = temp.path().join("source");
        std::fs::write(&source, b"original source bytes")?;
        let make = |modality| Candidate {
            session_number: 1,
            session_path: temp.path().to_path_buf(),
            series_uid: "test".into(),
            modality,
            description: String::new(),
            study_date: None,
            slice_count: 1,
            spacing_mm: [1.; 3],
            evidence: crate::ElectrodeEvidence::Unknown,
            reason: String::new(),
            eligible: true,
            suffix: Some(
                if modality == Modality::Mr {
                    "T1w"
                } else {
                    "CT"
                }
                .into(),
            ),
            suffix_reason: String::new(),
            preview: None,
            quality: 0.,
            explicit_preop: false,
            explicit_postop: false,
            slices: vec![crate::Slice {
                path: source.clone(),
                rows: 1,
                columns: 1,
                instance: 1,
                position: None,
                orientation: None,
                spacing: [1.; 2],
                thickness: 1.,
                frames: 1,
            }],
        };
        let scan = Scan {
            candidates: vec![make(Modality::Mr), make(Modality::Ct)],
            ..Scan::default()
        };
        let output = temp.path().join("prepared");
        let cancel = AtomicBool::new(false);
        let result = prepare_selection(&scan, 0, 1, &output, &cancel, |p| {
            if p.done > 0 {
                cancel.store(true, std::sync::atomic::Ordering::Relaxed)
            }
        });
        assert!(matches!(result, Err(Error::Cancelled)));
        assert!(!output.exists());
        assert_eq!(std::fs::read(&source)?, b"original source bytes");
        let existing = temp.path().join("existing");
        std::fs::create_dir(&existing)?;
        std::fs::write(existing.join("keep"), b"keep")?;
        cancel.store(false, std::sync::atomic::Ordering::Relaxed);
        assert!(matches!(
            prepare_selection(&scan, 0, 1, &existing, &cancel, |_| {}),
            Err(Error::OutputExists)
        ));
        assert!(existing.join("keep").is_file());
        Ok(())
    }
    #[test]
    fn invalid_selection_writes_nothing() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let out = temp.path().join("out");
        assert!(matches!(
            prepare_selection(
                &Scan::default(),
                0,
                1,
                &out,
                &AtomicBool::new(false),
                |_| {}
            ),
            Err(Error::Selection(_))
        ));
        assert!(!out.exists());
        Ok(())
    }
    #[test]
    fn sidecar_allowlist_excludes_identity_and_arbitrary_text(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let src = temp.path().join("raw.json");
        let dst = temp.path().join("safe.json");
        std::fs::write(
            &src,
            r#"{"PatientName":"SECRET","SeriesDescription":"SECRET","StudyDate":"20250101","EchoTime":0.003,"RepetitionTime":"SECRET","ImageOrientationPatientDICOM":[1,0,0,0,1,0],"Modality":"SECRET"}"#,
        )?;
        scrub_sidecar(Some(&src), &dst, "MR")?;
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(dst)?)?;
        assert_eq!(value["EchoTime"], 0.003);
        assert_eq!(value["Modality"], "MR");
        assert!(value.get("PatientName").is_none());
        assert!(value.get("StudyDate").is_none());
        assert!(value.get("RepetitionTime").is_none());
        assert!(!value.to_string().contains("SECRET"));
        Ok(())
    }
    #[test]
    fn malformed_sidecar_keeps_only_generic_modality() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let src = temp.path().join("raw.json");
        let dst = temp.path().join("safe.json");
        std::fs::write(
            &src,
            b"{\"EchoTime\":0.003,\"SeriesDescription\":\"private\x01text\"}",
        )?;
        scrub_sidecar(Some(&src), &dst, "MR")?;
        let value: serde_json::Value = serde_json::from_slice(&std::fs::read(dst)?)?;
        assert_eq!(value, serde_json::json!({"Modality":"MR"}));
        Ok(())
    }
    #[test]
    fn nifti_scrub_preserves_geometry_and_voxels_removes_text_and_extensions(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        let src = temp.path().join("source.nii");
        let dst = temp.path().join("safe.nii.gz");
        let mut bytes = vec![0u8; 372];
        bytes[..4].copy_from_slice(&348i32.to_le_bytes());
        bytes[108..112].copy_from_slice(&368f32.to_le_bytes());
        bytes[148..154].copy_from_slice(b"SECRET");
        bytes[280..284].copy_from_slice(&1.25f32.to_le_bytes());
        bytes[344..348].copy_from_slice(b"n+1\0");
        bytes[348] = 1;
        bytes[352..368].fill(17);
        bytes[368..372].copy_from_slice(&[1, 2, 3, 4]);
        std::fs::write(&src, bytes)?;
        scrub_nifti(&src, &dst, &AtomicBool::new(false))?;
        let mut safe = Vec::new();
        GzDecoder::new(File::open(dst)?).read_to_end(&mut safe)?;
        assert_eq!(safe.len(), 356);
        assert_eq!(&safe[352..], &[1, 2, 3, 4]);
        assert!(safe[148..252].iter().all(|b| *b == 0));
        assert_eq!(&safe[280..284], &1.25f32.to_le_bytes());
        assert_eq!(safe[348], 0);
        Ok(())
    }
}
