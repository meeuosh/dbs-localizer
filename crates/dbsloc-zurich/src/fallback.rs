// SPDX-FileCopyrightText: 2026 Wyss Center for Bio and Neuroengineering
// SPDX-License-Identifier: GPL-3.0-only

//! Broken-header fallbacks for MRI contrast classification:
//! - Tier 2: `dcm2niix` BIDS JSON sidecar physics (recovers vendor private tags).
//! - Tier 3: Pure 3D NIfTI intensity signature (works with zero headers).

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::Path;

use flate2::read::GzDecoder;

use crate::{ContrastSource, IntensityStats};

/// Infer MRI anatomical contrast (`T1w`, `T2w`, `FLAIR`, `PDw`) from a raw `dcm2niix` JSON sidecar
/// before metadata scrubbing. `dcm2niix` converts Siemens/Philips/GE private headers into standard
/// BIDS seconds-based `RepetitionTime`, `EchoTime`, and `InversionTime`.
pub(crate) fn infer_from_sidecar(sidecar_path: &Path) -> Option<(String, String)> {
    let file = File::open(sidecar_path).ok()?;
    let val: serde_json::Value = serde_json::from_reader(BufReader::new(file)).ok()?;
    let obj = val.as_object()?;

    // Check if sidecar flags non-anatomical acquisitions (DWI / perf / phase).
    if let Some(desc) = obj
        .get("PulseSequenceDetails")
        .or_else(|| obj.get("SequenceName"))
        .and_then(|v| v.as_str())
    {
        let low = desc.to_lowercase();
        if low.contains("ep2d_diff")
            || low.contains("dwi")
            || low.contains("dti")
            || low.contains("bold")
            || low.contains("asl")
        {
            return None;
        }
    }

    // BIDS sidecars store TR / TE / TI in seconds; convert to milliseconds.
    let tr_ms = obj
        .get("RepetitionTime")
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|s| if s < 50.0 { s * 1000.0 } else { s })?;
    let te_ms = obj
        .get("EchoTime")
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|s| if s < 5.0 { s * 1000.0 } else { s })?;
    let ti_ms = obj
        .get("InversionTime")
        .and_then(|v| v.as_f64())
        .filter(|v| v.is_finite() && *v > 0.0)
        .map(|s| if s < 20.0 { s * 1000.0 } else { s });
    let acq_type = obj
        .get("MRAcquisitionType")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();
    let seq = obj
        .get("ScanningSequence")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_uppercase();

    let suffix = classify_physics_ms(tr_ms, te_ms, ti_ms, &seq, &acq_type)?;
    let reason = format!(
        "Recovered from dcm2niix sidecar physics: TR={tr_ms:.1} ms, TE={te_ms:.1} ms, TI={ti_ms:?} ms, seq={seq}, dim={acq_type}"
    );
    Some((suffix.to_owned(), reason))
}

fn classify_physics_ms(
    tr: f64,
    te: f64,
    ti: Option<f64>,
    seq: &str,
    dim: &str,
) -> Option<&'static str> {
    if !(1.0..=25_000.0).contains(&tr) || !(0.5..=600.0).contains(&te) {
        return None;
    }
    let is_ir = seq.contains("IR") || ti.is_some();
    let is_gr = seq.contains("GR");
    let is_se = seq.contains("SE") || (!is_gr && tr >= 1500.0);

    if is_ir {
        if let Some(ti_val) = ti {
            if tr >= 4000.0 && te >= 60.0 && (1500.0..=3200.0).contains(&ti_val) {
                return Some("FLAIR");
            }
            if te <= 25.0 && (300.0..=1400.0).contains(&ti_val) {
                return Some("T1w");
            }
        }
    }
    if tr >= 2000.0 && te >= 65.0 && is_se {
        return Some("T2w");
    }
    if tr >= 1800.0 && te <= 35.0 && is_se && !is_ir {
        return Some("PDw");
    }
    if (tr <= 900.0 && te <= 30.0) || (is_gr && dim == "3D" && tr <= 50.0 && te <= 15.0) {
        return Some("T1w");
    }
    None
}

/// Tier 3 fallback: infer MRI contrast (`T1w`, `T2w`, `FLAIR`) purely from 3D NIfTI voxel
/// intensities when all DICOM and sidecar headers are missing or corrupted.
pub(crate) fn infer_from_nifti_intensity(
    nifti_path: &Path,
) -> Option<(String, String, IntensityStats)> {
    let (dims, voxels) = read_nifti1_f32(nifti_path)?;
    classify_volume_intensity(dims, &voxels)
}

/// Pure-math intensity classifier (separated for deterministic unit testing).
pub(crate) fn classify_volume_intensity(
    dims: [usize; 3],
    voxels: &[f32],
) -> Option<(String, String, IntensityStats)> {
    let [nx, ny, nz] = dims;
    if nx < 16 || ny < 16 || nz < 8 || voxels.len() < nx * ny * nz {
        return None;
    }

    // Collect foreground intensities from the central brain slab (25%..75% Z, 20%..80% XY).
    let mut brain_vals = Vec::new();
    let mut core_vals = Vec::new();
    let mut rim_vals = Vec::new();

    let z0 = nz / 4;
    let z1 = (nz * 3 / 4).max(z0 + 1);
    let y0 = ny / 5;
    let y1 = (ny * 4 / 5).max(y0 + 1);
    let x0 = nx / 5;
    let x1 = (nx * 4 / 5).max(x0 + 1);

    let cx0 = nx * 38 / 100;
    let cx1 = nx * 62 / 100;
    let cy0 = ny * 38 / 100;
    let cy1 = ny * 62 / 100;

    let stride_z = ((z1 - z0) / 16).max(1);
    let stride_xy = ((nx.min(ny)) / 64).max(1);

    for z in (z0..z1).step_by(stride_z) {
        for y in (y0..y1).step_by(stride_xy) {
            for x in (x0..x1).step_by(stride_xy) {
                let v = voxels[z * ny * nx + y * nx + x];
                if !v.is_finite() || v <= 0.0 {
                    continue;
                }
                brain_vals.push(v);
                if (cx0..=cx1).contains(&x) && (cy0..=cy1).contains(&y) {
                    core_vals.push(v);
                } else {
                    rim_vals.push(v);
                }
            }
        }
    }

    if brain_vals.len() < 64 || core_vals.len() < 16 || rim_vals.len() < 16 {
        return None;
    }

    brain_vals.sort_by(f32::total_cmp);
    // Drop background noise floor (below 10% of p95).
    let raw_p95 = percentile(&brain_vals, 0.95);
    let floor = raw_p95 * 0.10;
    brain_vals.retain(|&v| v > floor);
    core_vals.retain(|&v| v > floor * 0.25);
    rim_vals.retain(|&v| v > floor);

    if brain_vals.len() < 32 || core_vals.len() < 8 || rim_vals.len() < 8 {
        return None;
    }
    brain_vals.sort_by(f32::total_cmp);
    core_vals.sort_by(f32::total_cmp);
    rim_vals.sort_by(f32::total_cmp);

    let p50 = percentile(&brain_vals, 0.50).max(1e-6);
    let p90 = percentile(&brain_vals, 0.90);
    let p99 = percentile(&brain_vals, 0.99);
    let core_p90 = percentile(&core_vals, 0.90);
    let core_p20 = percentile(&core_vals, 0.20);
    let rim_p75 = percentile(&rim_vals, 0.75);

    let csf_bright_ratio = core_p90 / p50;
    let csf_dark_ratio = core_p20 / p50;
    let tail_ratio = p99 / p50;
    let rim_to_median = rim_p75 / p50;

    let stats = IntensityStats {
        brain_median: p50,
        p90_ratio: p90 / p50,
        p99_ratio: tail_ratio,
        core_bright_ratio: csf_bright_ratio,
        core_dark_ratio: csf_dark_ratio,
        rim_to_median,
    };

    // T2w: CSF in central ventricles is markedly brighter than parenchymal median.
    if csf_bright_ratio >= 1.45 && tail_ratio >= 1.85 {
        return Some((
            "T2w".to_owned(),
            format!(
                "Image-intensity fallback (Tier 3): bright central CSF (core_p90/median={csf_bright_ratio:.2}, p99/p50={tail_ratio:.2}); verify visually"
            ),
            stats,
        ));
    }

    // T1w vs FLAIR: both have dark CSF; T1w has bright scalp/WM with higher upper-rim contrast
    // relative to median than FLAIR's flatter parenchyma histogram.
    if csf_dark_ratio <= 0.60 {
        if rim_to_median >= 1.12 {
            return Some((
                "T1w".to_owned(),
                format!(
                    "Image-intensity fallback (Tier 3): dark central CSF + bright parenchyma/rim (core_p20/median={csf_dark_ratio:.2}, rim/median={rim_to_median:.2}); verify visually"
                ),
                stats,
            ));
        }
        return Some((
            "FLAIR".to_owned(),
            format!(
                "Image-intensity fallback (Tier 3): suppressed CSF + flatter parenchyma contrast (core_p20/median={csf_dark_ratio:.2}, rim/median={rim_to_median:.2}); verify visually"
            ),
            stats,
        ));
    }

    None
}

/// Returns `(ContrastSource, Option<(suffix, reason)>, Option<IntensityStats>)` using Tier 2
/// (`dcm2niix` sidecar) then Tier 3 (NIfTI 3D intensity).
pub(crate) fn fallback_mri_contrast(
    raw_sidecar: Option<&Path>,
    converted_nifti: &Path,
) -> (
    ContrastSource,
    Option<(String, String)>,
    Option<IntensityStats>,
) {
    if let Some(sidecar) = raw_sidecar {
        if let Some(found) = infer_from_sidecar(sidecar) {
            return (ContrastSource::SidecarPhysics, Some(found), None);
        }
    }
    if let Some((suffix, reason, stats)) = infer_from_nifti_intensity(converted_nifti) {
        return (
            ContrastSource::ImageIntensity,
            Some((suffix, reason)),
            Some(stats),
        );
    }
    (ContrastSource::Unknown, None, None)
}

fn percentile(sorted: &[f32], q: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() - 1) as f32 * q.clamp(0.0, 1.0)).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

/// Minimal pure-Rust NIfTI-1 voxel reader supporting common integer & float datatypes.
fn read_nifti1_f32(path: &Path) -> Option<([usize; 3], Vec<f32>)> {
    let file = File::open(path).ok()?;
    let mut reader: Box<dyn Read> = if path.extension().is_some_and(|e| e == "gz") {
        Box::new(GzDecoder::new(BufReader::new(file)))
    } else {
        Box::new(BufReader::new(file))
    };
    let mut h = [0u8; 348];
    reader.read_exact(&mut h).ok()?;
    let little = match i32::from_le_bytes([h[0], h[1], h[2], h[3]]) {
        348 => true,
        _ if i32::from_be_bytes([h[0], h[1], h[2], h[3]]) == 348 => false,
        _ => return None,
    };
    let read_i16 = |off: usize| -> i16 {
        let b = [h[off], h[off + 1]];
        if little {
            i16::from_le_bytes(b)
        } else {
            i16::from_be_bytes(b)
        }
    };
    let read_f32 = |off: usize| -> f32 {
        let b = [h[off], h[off + 1], h[off + 2], h[off + 3]];
        if little {
            f32::from_le_bytes(b)
        } else {
            f32::from_be_bytes(b)
        }
    };

    let ndim = read_i16(40);
    if ndim < 3 {
        return None;
    }
    let nx = usize::try_from(read_i16(42)).ok()?;
    let ny = usize::try_from(read_i16(44)).ok()?;
    let nz = usize::try_from(read_i16(46)).ok()?;
    let count = nx.checked_mul(ny)?.checked_mul(nz)?;
    if count == 0 || count > 64_000_000 {
        return None;
    }

    let datatype = read_i16(70);
    let vox_offset = read_f32(108);
    if !vox_offset.is_finite() || vox_offset < 348.0 {
        return None;
    }
    let slope = {
        let s = read_f32(112);
        if s.is_finite() && s != 0.0 {
            s
        } else {
            1.0
        }
    };
    let inter = {
        let i = read_f32(116);
        if i.is_finite() {
            i
        } else {
            0.0
        }
    };

    let skip = (vox_offset as usize).saturating_sub(348);
    if skip > 0 {
        let mut discard = vec![0u8; skip];
        reader.read_exact(&mut discard).ok()?;
    }

    let bpp = match datatype {
        2 | 256 => 1usize,
        4 | 512 => 2usize,
        8 | 16 | 768 => 4usize,
        64 => 8usize,
        _ => return None,
    };
    let mut raw = vec![0u8; count.checked_mul(bpp)?];
    reader.read_exact(&mut raw).ok()?;

    let mut out = Vec::with_capacity(count);
    for chunk in raw.chunks_exact(bpp) {
        let v = match datatype {
            2 => chunk[0] as f32,
            256 => (chunk[0] as i8) as f32,
            4 => {
                let b = [chunk[0], chunk[1]];
                (if little {
                    i16::from_le_bytes(b)
                } else {
                    i16::from_be_bytes(b)
                }) as f32
            }
            512 => {
                let b = [chunk[0], chunk[1]];
                (if little {
                    u16::from_le_bytes(b)
                } else {
                    u16::from_be_bytes(b)
                }) as f32
            }
            8 => {
                let b = [chunk[0], chunk[1], chunk[2], chunk[3]];
                (if little {
                    i32::from_le_bytes(b)
                } else {
                    i32::from_be_bytes(b)
                }) as f32
            }
            16 => {
                let b = [chunk[0], chunk[1], chunk[2], chunk[3]];
                if little {
                    f32::from_le_bytes(b)
                } else {
                    f32::from_be_bytes(b)
                }
            }
            768 => {
                let b = [chunk[0], chunk[1], chunk[2], chunk[3]];
                (if little {
                    u32::from_le_bytes(b)
                } else {
                    u32::from_be_bytes(b)
                }) as f32
            }
            64 => {
                let b = [
                    chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
                ];
                (if little {
                    f64::from_le_bytes(b)
                } else {
                    f64::from_be_bytes(b)
                }) as f32
            }
            _ => return None,
        };
        out.push(v * slope + inter);
    }

    Some(([nx, ny, nz], out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sidecar_recovers_t1w_t2w_and_flair_in_seconds() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let t1 = dir.path().join("t1.json");
        std::fs::write(
            &t1,
            r#"{"RepetitionTime":2.3,"EchoTime":0.0029,"InversionTime":0.9,"ScanningSequence":"GR\\IR","MRAcquisitionType":"3D"}"#,
        )?;
        assert_eq!(infer_from_sidecar(&t1).map(|v| v.0).as_deref(), Some("T1w"));

        let t2 = dir.path().join("t2.json");
        std::fs::write(
            &t2,
            r#"{"RepetitionTime":4.5,"EchoTime":0.095,"ScanningSequence":"SE","MRAcquisitionType":"2D"}"#,
        )?;
        assert_eq!(infer_from_sidecar(&t2).map(|v| v.0).as_deref(), Some("T2w"));

        let flair = dir.path().join("flair.json");
        std::fs::write(
            &flair,
            r#"{"RepetitionTime":9.0,"EchoTime":0.09,"InversionTime":2.5,"ScanningSequence":"SE\\IR","MRAcquisitionType":"2D"}"#,
        )?;
        assert_eq!(
            infer_from_sidecar(&flair).map(|v| v.0).as_deref(),
            Some("FLAIR")
        );
        Ok(())
    }

    #[test]
    fn intensity_fallback_separates_t2w_and_t1w() {
        let dims = [32, 32, 16];
        let total = 32 * 32 * 16;

        // Synthetic T2w: parenchyma ~ 100, bright central CSF ventricles ~ 260.
        let mut t2 = vec![100.0f32; total];
        for z in 4..12 {
            for y in 13..19 {
                for x in 13..19 {
                    t2[z * 32 * 32 + y * 32 + x] = 260.0;
                }
            }
        }
        let (suffix, _, _) = classify_volume_intensity(dims, &t2).expect("T2w detected");
        assert_eq!(suffix, "T2w");

        // Synthetic T1w: parenchyma ~ 100, dark central CSF ~ 20, bright outer scalp/WM rim ~ 150.
        let mut t1 = vec![100.0f32; total];
        for z in 4..12 {
            for y in 6..26 {
                for x in 6..26 {
                    let idx = z * 32 * 32 + y * 32 + x;
                    if (13..19).contains(&x) && (13..19).contains(&y) {
                        t1[idx] = 20.0;
                    } else if x <= 7 || x >= 24 || y <= 7 || y >= 24 {
                        t1[idx] = 150.0;
                    }
                }
            }
        }
        let (suffix, _, _) = classify_volume_intensity(dims, &t1).expect("T1w detected");
        assert_eq!(suffix, "T1w");
    }
}
