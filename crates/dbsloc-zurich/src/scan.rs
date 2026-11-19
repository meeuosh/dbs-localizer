// SPDX-FileCopyrightText: 2026 Wyss Center for Bio and Neuroengineering
// SPDX-License-Identifier: GPL-3.0-only

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use dicom_dictionary_std::tags;
use dicom_object::{DefaultDicomObject, OpenFileOptions};
use dicom_pixeldata::PixelDecoder;
use rayon::prelude::*;

use crate::{
    check_cancel, io, reject_traversal, Candidate, ElectrodeEvidence, Error, Modality, Preview,
    Progress, Scan, Slice,
};

fn session_number(name: &str) -> Option<u32> {
    let suffix = name.strip_prefix("export-").unwrap_or(name);
    if suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    suffix.parse().ok().filter(|n| *n > 0)
}

fn modality(name: &str) -> Option<Modality> {
    match name.to_ascii_uppercase().as_str() {
        "CT" => Some(Modality::Ct),
        "MR" | "MRI" => Some(Modality::Mr),
        _ => None,
    }
}

fn valid_date(raw: &str) -> bool {
    if raw.len() != 8 || !raw.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    let year = raw[..4].parse::<u32>().unwrap_or(0);
    let month = raw[4..6].parse::<u32>().unwrap_or(0);
    let day = raw[6..].parse::<u32>().unwrap_or(0);
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    };
    year >= 1800 && (1..=days).contains(&day)
}

fn children(path: &Path) -> Result<Vec<(PathBuf, std::fs::FileType)>, Error> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(path).map_err(|e| io("read directory", e))? {
        let entry = entry.map_err(|e| io("read directory entry", e))?;
        let ty = entry.file_type().map_err(|e| io("read entry type", e))?;
        if !ty.is_symlink() {
            out.push((entry.path(), ty));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn viewer_directory(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    matches!(
        name.as_str(),
        "resource"
            | "help"
            | "configuration"
            | "3rdparty"
            | "modules"
            | "plugins"
            | "deepunitymediaviewermac"
            | "jre"
    ) || name.starts_with('.')
}

fn walk(path: &Path, out: &mut Vec<PathBuf>, cancel: &AtomicBool) -> Result<(), Error> {
    check_cancel(cancel)?;
    for (p, ty) in children(path)? {
        if ty.is_dir() && !viewer_directory(&p) {
            walk(&p, out, cancel)?;
        } else if ty.is_file() {
            let ext = p
                .extension()
                .map(|s| s.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            if ext.is_empty() || matches!(ext.as_str(), "dcm" | "dicom" | "ima") {
                out.push(p);
            }
        }
    }
    Ok(())
}

fn text(obj: &DefaultDicomObject, tag: dicom_object::Tag) -> String {
    obj.element(tag)
        .ok()
        .and_then(|e| e.to_str().ok())
        .map(|v| v.trim_matches(['\0', ' ']).to_owned())
        .unwrap_or_default()
}

fn number(obj: &DefaultDicomObject, tag: dicom_object::Tag) -> Option<f64> {
    obj.element(tag)
        .ok()?
        .to_float64()
        .ok()
        .filter(|n| n.is_finite())
}

fn vector<const N: usize>(obj: &DefaultDicomObject, tag: dicom_object::Tag) -> Option<[f64; N]> {
    let vals = obj.element(tag).ok()?.to_multi_float64().ok()?;
    let a: [f64; N] = vals.try_into().ok()?;
    a.iter().all(|v| v.is_finite()).then_some(a)
}

struct Header {
    uid: String,
    sop_uid: String,
    modality: Modality,
    description: String,
    protocol: String,
    image_type: String,
    date: Option<String>,
    acquisition: Acquisition,
    slice: Slice,
}

fn header(path: &Path, hint: Modality) -> Option<Header> {
    let obj = OpenFileOptions::new()
        .read_until(tags::PIXEL_DATA)
        .open_file(path)
        .ok()?;
    let uid = text(&obj, tags::SERIES_INSTANCE_UID);
    if uid.is_empty() {
        return None;
    }
    let tag_modality = text(&obj, tags::MODALITY);
    let m = if tag_modality.is_empty() {
        hint
    } else {
        modality(&tag_modality)?
    };
    let rows = number(&obj, tags::ROWS)? as usize;
    let columns = number(&obj, tags::COLUMNS)? as usize;
    if rows == 0 || columns == 0 || rows > 2048 || columns > 2048 {
        return None;
    }
    let date = text(&obj, tags::STUDY_DATE);
    let date = valid_date(&date).then_some(date);
    let spacing = vector::<2>(&obj, tags::PIXEL_SPACING).unwrap_or([0.; 2]);
    Some(Header {
        uid,
        sop_uid: text(&obj, tags::SOP_INSTANCE_UID),
        modality: m,
        description: text(&obj, tags::SERIES_DESCRIPTION),
        protocol: format!(
            "{} {}",
            text(&obj, tags::PROTOCOL_NAME),
            text(&obj, tags::SEQUENCE_NAME)
        ),
        image_type: text(&obj, tags::IMAGE_TYPE),
        date,
        acquisition: Acquisition::read(&obj),
        slice: Slice {
            path: path.to_path_buf(),
            rows,
            columns,
            instance: number(&obj, tags::INSTANCE_NUMBER).unwrap_or(0.) as i32,
            position: vector(&obj, tags::IMAGE_POSITION_PATIENT),
            orientation: vector(&obj, tags::IMAGE_ORIENTATION_PATIENT),
            spacing: [spacing[0] as f32, spacing[1] as f32],
            thickness: number(&obj, tags::SLICE_THICKNESS).unwrap_or(0.) as f32,
            frames: number(&obj, tags::NUMBER_OF_FRAMES).unwrap_or(1.) as u32,
        },
    })
}

fn projected(slice: &Slice, orientation: [f64; 6]) -> Option<f64> {
    let p = slice.position?;
    let n = [
        orientation[1] * orientation[5] - orientation[2] * orientation[4],
        orientation[2] * orientation[3] - orientation[0] * orientation[5],
        orientation[0] * orientation[4] - orientation[1] * orientation[3],
    ];
    Some(p.iter().zip(n).map(|(p, n)| p * n).sum())
}

pub(crate) fn preview(slice: &Slice) -> Option<Preview> {
    if slice.rows > 2048 || slice.columns > 2048 || slice.frames != 1 {
        return None;
    }
    if std::fs::metadata(&slice.path).ok()?.len() > 64 * 1024 * 1024 {
        return None;
    }
    let obj = dicom_object::open_file(&slice.path).ok()?;
    let frames = number(&obj, tags::NUMBER_OF_FRAMES).unwrap_or(1.);
    if frames != 1.
        || number(&obj, tags::ROWS)? as usize != slice.rows
        || number(&obj, tags::COLUMNS)? as usize != slice.columns
        || number(&obj, tags::SAMPLES_PER_PIXEL).unwrap_or(1.) != 1.
    {
        return None;
    }
    let decoded = obj.decode_pixel_data().ok()?;
    // `to_vec::<f32>` applies the DICOM Modality LUT (slope/intercept) exactly once.
    let pixels = decoded.to_vec::<f32>().ok()?;
    (pixels.len() == slice.rows * slice.columns).then_some(Preview {
        width: slice.columns,
        height: slice.rows,
        pixels,
    })
}

fn thumbnail(p: Preview) -> Preview {
    let factor = p.width.max(p.height).div_ceil(256).max(1);
    if factor == 1 {
        return p;
    }
    let width = p.width.div_ceil(factor);
    let height = p.height.div_ceil(factor);
    let pixels = (0..height)
        .flat_map(|y| (0..width).map(move |x| (y * factor, x * factor)))
        .map(|(y, x)| p.pixels[y * p.width + x])
        .collect();
    Preview {
        width,
        height,
        pixels,
    }
}

fn any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| haystack.contains(n))
}

#[derive(Clone, Debug, Default)]
struct Acquisition {
    tr: Option<f64>,
    te: Option<f64>,
    ti: Option<f64>,
    flip: Option<f64>,
    family: String,
    variant: String,
    dimensionality: String,
    diffusion: bool,
    b_value: Option<f64>,
    diffusion_direction: String,
    phase: bool,
    perfusion_or_functional: bool,
    invalid: bool,
}

impl Acquisition {
    fn read(obj: &DefaultDicomObject) -> Self {
        let tr = number(obj, tags::REPETITION_TIME);
        let te = number(obj, tags::ECHO_TIME);
        let ti = number(obj, tags::INVERSION_TIME);
        let flip = number(obj, tags::FLIP_ANGLE);
        let invalid = [
            (tags::REPETITION_TIME, tr),
            (tags::ECHO_TIME, te),
            (tags::INVERSION_TIME, ti),
            (tags::FLIP_ANGLE, flip),
        ]
        .iter()
        .any(|(tag, value)| obj.element(*tag).is_ok() && value.is_none());
        let direction = text(obj, tags::DIFFUSION_DIRECTIONALITY).to_ascii_uppercase();
        let phase = text(obj, tags::COMPLEX_IMAGE_COMPONENT).eq_ignore_ascii_case("PHASE");
        let asl = text(obj, tags::ARTERIAL_SPIN_LABELING_CONTRAST);
        let perfusion_or_functional = (!asl.is_empty() && !asl.eq_ignore_ascii_case("NONE"))
            || obj
                .element(tags::MR_ARTERIAL_SPIN_LABELING_SEQUENCE)
                .is_ok()
            || obj.element(tags::FUNCTIONAL_MR_SEQUENCE).is_ok();
        Self {
            tr,
            te,
            ti,
            flip,
            family: text(obj, tags::SCANNING_SEQUENCE).to_ascii_uppercase(),
            variant: text(obj, tags::SEQUENCE_VARIANT).to_ascii_uppercase(),
            dimensionality: text(obj, tags::MR_ACQUISITION_TYPE).to_ascii_uppercase(),
            diffusion: number(obj, tags::DIFFUSION_B_VALUE).is_some_and(|b| b > 0.)
                || (!direction.is_empty() && direction != "NONE"),
            b_value: number(obj, tags::DIFFUSION_B_VALUE),
            diffusion_direction: match direction.as_str() {
                "DIRECTIONAL" | "ISOTROPIC" | "NONE" => direction,
                _ => String::new(),
            },
            phase,
            perfusion_or_functional,
            invalid,
        }
    }
    fn family(&self, value: &str) -> bool {
        self.family.split(['\\', ' ', ',']).any(|v| v == value)
    }
    fn summary(&self) -> String {
        format!("TR={:?} ms, TE={:?} ms, TI={:?} ms, flip={:?} deg, family={}, variant={}, acquisition={}",self.tr,self.te,self.ti,self.flip,if self.family.is_empty(){"unknown"}else{&self.family},if self.variant.is_empty(){"unknown"}else{&self.variant},if self.dimensionality.is_empty(){"unknown"}else{&self.dimensionality})
    }
}

fn named_suffix(label: &str) -> Option<String> {
    let s = if any(label, &["fgatir"]) {
        "FGATIR"
    } else if any(label, &["flair"]) {
        "FLAIR"
    } else if any(label, &["t2star", "t2*"]) {
        "T2starw"
    } else if any(label, &["pdw", "proton"]) {
        "PDw"
    } else if any(label, &["t2", "space"]) {
        "T2w"
    } else if any(label, &["mprage", "mp2rage", "t1", "bravo", "spgr", "tfe"]) {
        "T1w"
    } else {
        return None;
    };
    Some(s.to_owned())
}

fn infer_suffix(label: &str, a: &Acquisition) -> (Option<String>, String) {
    let phase_token = label
        .split(|c: char| !c.is_ascii_alphanumeric())
        .any(|v| matches!(v, "p" | "phase" | "phasediff" | "phase1" | "phase2"));
    if a.diffusion
        || a.phase
        || a.perfusion_or_functional
        || a.family("EP")
        || phase_token
        || any(
            label,
            &[
                "diffusion",
                "dwi",
                "dti",
                "adc",
                "tracew",
                "perfusion",
                "asl",
                "dsc",
                "functional",
                "bold",
                "fieldmap",
                "field_map",
                "field map",
                "suscept",
                "swi",
            ],
        )
    {
        let known_keyword = [
            "diffusion",
            "dwi",
            "dti",
            "adc",
            "tracew",
            "perfusion",
            "asl",
            "dsc",
            "functional",
            "bold",
            "fieldmap",
            "field_map",
            "field map",
            "suscept",
            "swi",
        ]
        .into_iter()
        .find(|kw| label.contains(kw));
        return (None,format!("Non-anatomical acquisition flags: diffusion={} b-value={:?} direction={}; phase={} phase-token={}; perfusion/functional={}; EP={}; keyword={:?}; {}",a.diffusion,a.b_value,a.diffusion_direction,a.phase,phase_token,a.perfusion_or_functional,a.family("EP"),known_keyword,a.summary()));
    }
    if let Some(s) = named_suffix(label) {
        return (
            Some(s.clone()),
            format!("Recognized anatomical sequence label: {s}; review required"),
        );
    }
    let unknown = || {
        (None,format!("Insufficient or ambiguous acquisition parameters: {}; assign anatomical type manually",a.summary()))
    };
    if a.invalid {
        return unknown();
    }
    if a.variant.split(['\\', ' ', ',']).any(|v| v == "SS") {
        return unknown();
    }
    let (Some(tr), Some(te)) = (a.tr, a.te) else {
        return unknown();
    };
    let ti = a.ti.unwrap_or(0.);
    if !tr.is_finite()
        || !te.is_finite()
        || !ti.is_finite()
        || !(1.0..=60000.0).contains(&tr)
        || !(0.1..=1000.0).contains(&te)
        || ti < 0.
        || ti > tr
        || ti > 20000.
        || te > tr
        || a.flip
            .is_some_and(|f| !f.is_finite() || f <= 0. || f > 180.)
    {
        return unknown();
    }
    let kind = if tr >= 4000. && te >= 60. && ti >= 1500. {
        Some("FLAIR")
    } else if (200.0..=1400.0).contains(&ti) && te <= 15. && tr >= ti {
        Some("T1w")
    } else if tr >= 1500. && te >= 60. && ti == 0. && !a.family("GR") {
        Some("T2w")
    } else if tr >= 1500. && te <= 30. && ti == 0. && a.family("SE") && !a.family("GR") {
        Some("PDw")
    } else if ((100.0..=1000.0).contains(&tr)
        && te <= 30.
        && ti == 0.
        && a.family("SE")
        && !a.family("GR"))
        || (tr <= 50.
            && te <= 10.
            && ti == 0.
            && a.dimensionality == "3D"
            && a.family("GR")
            && !a.family("SE"))
    {
        Some("T1w")
    } else {
        None
    };
    match kind {
        Some(s) => (
            Some(s.into()),
            format!(
                "Acquisition-parameter guess {s}: {}; review required",
                a.summary()
            ),
        ),
        None => unknown(),
    }
}

fn build(mut headers: Vec<Header>, session_number: u32, session_path: PathBuf) -> Candidate {
    let mut seen = std::collections::HashSet::new();
    headers.retain(|h| h.sop_uid.is_empty() || seen.insert(h.sop_uid.clone()));
    let orientation = headers.first().and_then(|h| h.slice.orientation);
    if let Some(o) = orientation {
        if headers.iter().all(|h| projected(&h.slice, o).is_some()) {
            headers.sort_by(|a, b| {
                projected(&a.slice, o)
                    .partial_cmp(&projected(&b.slice, o))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.slice.path.cmp(&b.slice.path))
            });
        } else {
            headers.sort_by_key(|h| h.slice.instance);
        }
    } else {
        headers.sort_by_key(|h| h.slice.instance);
    }
    let first = &headers[0];
    let label = format!(
        "{} {} {}",
        first.description, first.protocol, first.image_type
    )
    .to_lowercase();
    let scout = headers.len() < 20
        || any(
            &label,
            &[
                "scout",
                "localizer",
                "localiser",
                "topogram",
                "surview",
                "dose",
                "report",
                "screenshot",
                "save_screen",
                "mip",
                "minip",
                "bone",
                "knochen",
            ],
        );
    let mut consistent = headers.iter().all(|h| {
        h.slice.frames == 1
            && h.modality == first.modality
            && h.slice.rows == first.slice.rows
            && h.slice.columns == first.slice.columns
            && h.slice
                .spacing
                .iter()
                .zip(first.slice.spacing)
                .all(|(a, b)| (*a - b).abs() < 0.01)
    });
    if let Some(o) = orientation {
        let row_norm = o[..3].iter().map(|v| v * v).sum::<f64>();
        let col_norm = o[3..].iter().map(|v| v * v).sum::<f64>();
        let dot = o[..3].iter().zip(&o[3..]).map(|(a, b)| a * b).sum::<f64>();
        consistent &=
            (row_norm - 1.).abs() < 0.02 && (col_norm - 1.).abs() < 0.02 && dot.abs() < 0.02;
        consistent &= headers.iter().all(|h| {
            h.slice
                .orientation
                .is_some_and(|other| other.iter().zip(o).all(|(a, b)| (*a - b).abs() < 0.01))
        });
        let positions: Vec<_> = headers
            .iter()
            .filter_map(|h| projected(&h.slice, o))
            .collect();
        if positions.len() == headers.len() {
            let steps: Vec<_> = positions.windows(2).map(|v| v[1] - v[0]).collect();
            let reference = steps.first().copied().unwrap_or(0.);
            consistent &= reference > 0.0001 && steps.iter().all(|v| (*v - reference).abs() < 0.05);
        }
    }
    consistent &= headers.len() <= 2048;
    let z = orientation
        .and_then(|o| {
            projected(&headers[headers.len() - 1].slice, o).zip(projected(&first.slice, o))
        })
        .filter(|_| headers.len() > 1)
        .map(|(a, b)| ((a - b).abs() / (headers.len() - 1) as f64) as f32)
        .filter(|z| *z > 0.)
        .unwrap_or(first.slice.thickness);
    let spacing = [first.slice.spacing[1], first.slice.spacing[0], z];
    let (recognized, suffix_reason) = if first.modality == Modality::Ct {
        (
            Some("CT".into()),
            "CT modality from DICOM tag or recognized CT folder".into(),
        )
    } else {
        infer_suffix(&label, &first.acquisition)
    };
    let eligible = !scout && consistent && spacing.iter().all(|s| s.is_finite() && *s > 0.);
    let post = any(
        &label,
        &[
            "postop",
            "post-op",
            "post_op",
            "dbs",
            "electrode",
            "elektrode",
            "implant",
            "lead",
        ],
    );
    let pre = any(&label, &["preop", "pre-op", "pre_op", "praep", "präop"]);
    let reason = if !consistent {
        "Unsupported multiframe, repeated positions or inconsistent slice geometry"
    } else if scout {
        "Scout, derived image or fewer than 20 slices"
    } else if recognized.is_none() {
        "Unrecognized MRI anatomical type; assign it manually before using this volume"
    } else {
        "Electrode status has not been established"
    }
    .to_owned();
    Candidate {
        session_number,
        session_path,
        series_uid: first.uid.clone(),
        modality: first.modality,
        description: first.description.clone(),
        study_date: headers
            .iter()
            .all(|h| h.date == first.date)
            .then(|| first.date.clone())
            .flatten(),
        slice_count: headers.len(),
        spacing_mm: spacing,
        evidence: ElectrodeEvidence::Unknown,
        reason,
        eligible,
        suffix: recognized,
        suffix_reason,
        preview: None,
        quality: 1. / z.max(0.1) + headers.len().min(500) as f32 / 500.,
        explicit_preop: pre,
        explicit_postop: post,
        slices: headers.into_iter().map(|h| h.slice).collect(),
    }
}

fn evidence(candidate: &mut Candidate, cancel: &AtomicBool) -> Result<(), Error> {
    check_cancel(cancel)?;
    candidate.preview = candidate
        .slices
        .get(candidate.slices.len() / 2)
        .and_then(preview);
    if !candidate.eligible {
        return Ok(());
    }
    if candidate.modality == Modality::Mr {
        return Ok(());
    }
    let axial = candidate.slices.iter().all(|s| {
        s.position.is_some()
            && s.orientation
                .is_some_and(|o| (o[0] * o[4] - o[1] * o[3]).abs() > 0.8)
    });
    if !axial {
        candidate.reason =
            "CT sparse detector needs reliable near-axial geometry; review manually".into();
        return Ok(());
    }
    let mut superior: Vec<usize> = (0..candidate.slices.len()).collect();
    superior.sort_by(|a, b| {
        candidate.slices[*a]
            .position
            .map(|p| p[2])
            .partial_cmp(&candidate.slices[*b].position.map(|p| p[2]))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut indices: Vec<usize> = [0.45, 0.54, 0.62, 0.71, 0.80]
        .into_iter()
        .map(|f| superior[((candidate.slices.len() - 1) as f64 * f).round() as usize])
        .collect();
    indices.dedup();
    let mut positive = 0;
    let mut failed = 0;
    for i in indices {
        check_cancel(cancel)?;
        let p = if i == candidate.slices.len() / 2 {
            candidate.preview.clone()
        } else {
            preview(&candidate.slices[i])
        };
        let Some(p) = p else {
            failed += 1;
            continue;
        };
        let mut metal = 0;
        for y in p.height / 4..p.height * 3 / 4 {
            for x in p.width / 4..p.width * 3 / 4 {
                if p.pixels[y * p.width + x] >= 2500. {
                    metal += 1;
                }
            }
        }
        if (2..=450).contains(&metal) {
            positive += 1;
        }
    }
    if failed > 0 {
        candidate.reason =
            "One or more sampled CT slices could not be decoded; evidence unknown".into();
    } else if positive >= 2 {
        candidate.evidence = ElectrodeEvidence::Detected;
        candidate.reason = format!(
            "Metal-like central clusters on {positive}/5 CT samples (>=2500 HU); review required"
        );
    } else {
        candidate.evidence = ElectrodeEvidence::LikelyAbsent;
        candidate.reason =
            "Sparse CT sampling found no consistent metal signature; absence is not proven".into();
    }
    Ok(())
}

pub(crate) fn recommend(scan: &mut Scan) {
    let first_ct = scan
        .candidates
        .iter()
        .filter(|c| {
            c.eligible && c.modality == Modality::Ct && c.evidence == ElectrodeEvidence::Detected
        })
        .filter_map(|c| c.study_date.as_ref())
        .min()
        .cloned();
    for c in &mut scan.candidates {
        if c.modality != Modality::Mr || !c.eligible {
            continue;
        }
        if c.explicit_postop {
            c.evidence = ElectrodeEvidence::Detected;
            c.reason = "Post-op/DBS metadata; keyword evidence only, review required".into();
        } else if c
            .study_date
            .as_ref()
            .zip(first_ct.as_ref())
            .is_some_and(|(m, ct)| m >= ct)
        {
            c.evidence = ElectrodeEvidence::Unknown;
            c.reason =
                "MRI acquired on/after earliest metal-positive CT; cannot suggest pre-op".into();
        } else if c
            .study_date
            .as_ref()
            .zip(first_ct.as_ref())
            .is_some_and(|(m, ct)| m < ct)
        {
            c.evidence = ElectrodeEvidence::LikelyAbsent;
            c.reason="MRI predates earliest metal-positive CT; chronological suggestion, review required".into();
        } else if c.explicit_preop {
            c.evidence = ElectrodeEvidence::LikelyAbsent;
            c.reason = "Explicit pre-op metadata; electrode absence is not proven".into();
        }
    }
    let choose = |modality, evidence| {
        scan.candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| {
                c.modality == modality && c.eligible && c.evidence == evidence && c.suffix.is_some()
            })
            .min_by(|(_, a), (_, b)| {
                b.session_number
                    .cmp(&a.session_number)
                    .then_with(|| {
                        (b.suffix.as_deref() == Some("T1w"))
                            .cmp(&(a.suffix.as_deref() == Some("T1w")))
                    })
                    .then_with(|| b.quality.total_cmp(&a.quality))
                    .then_with(|| a.series_uid.cmp(&b.series_uid))
            })
            .map(|(i, _)| i)
    };
    scan.preop_mri = choose(Modality::Mr, ElectrodeEvidence::LikelyAbsent);
    scan.postop_ct = choose(Modality::Ct, ElectrodeEvidence::Detected);
}

pub fn scan_patient(
    path: &Path,
    cancel: &AtomicBool,
    progress: impl Fn(Progress) + Sync,
) -> Result<Scan, Error> {
    check_cancel(cancel)?;
    reject_traversal(path)?;
    let root = std::fs::canonicalize(path).map_err(|e| io("open patient directory", e))?;
    let mut sessions = Vec::new();
    for (dir, ty) in children(&root)? {
        if !ty.is_dir() {
            continue;
        }
        let Some(m) = dir.file_name().and_then(|s| s.to_str()).and_then(modality) else {
            continue;
        };
        for (sdir, ty) in children(&dir)? {
            if ty.is_dir() {
                if let Some(n) = sdir
                    .file_name()
                    .and_then(|s| s.to_str())
                    .and_then(session_number)
                {
                    sessions.push((sdir, n, m));
                }
            }
        }
    }
    if sessions.is_empty() {
        return Err(Error::NoSessions);
    }
    let mut files = Vec::new();
    for (si, (dir, _, _)) in sessions.iter().enumerate() {
        let mut paths = Vec::new();
        walk(dir, &mut paths, cancel)?;
        files.extend(paths.into_iter().map(|p| (si, p)));
    }
    let total = files.len();
    progress(Progress {
        done: 0,
        total,
        label: "Indexing DICOM headers".into(),
    });
    let done = AtomicUsize::new(0);
    let headers: Vec<_> = files
        .par_iter()
        .filter_map(|(si, p)| {
            if cancel.load(Ordering::Relaxed) {
                return None;
            }
            let h = header(p, sessions[*si].2);
            let n = done.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_multiple_of(128) || n == total {
                progress(Progress {
                    done: n,
                    total,
                    label: "Indexing DICOM headers".into(),
                });
            }
            h.map(|h| (*si, h))
        })
        .collect();
    check_cancel(cancel)?;
    let mut groups: BTreeMap<(usize, String), Vec<Header>> = BTreeMap::new();
    for (si, h) in headers {
        groups.entry((si, h.uid.clone())).or_default().push(h);
    }
    let mut scan = Scan::default();
    if groups.is_empty() {
        return Err(Error::NoSeries);
    }
    let total = groups.len();
    for ((si, _), headers) in groups {
        let mut c = build(headers, sessions[si].1, sessions[si].0.clone());
        evidence(&mut c, cancel)?;
        c.preview = c.preview.map(thumbnail);
        scan.candidates.push(c);
        progress(Progress {
            done: scan.candidates.len(),
            total,
            label: "Checking scan evidence".into(),
        });
    }
    recommend(&mut scan);
    scan.warnings.push("Electrode suggestions use sparse CT sampling and MRI chronology/metadata. Review the images before import.".into());
    if scan.preop_mri.is_none() {
        scan.warnings.push("No eligible pre-op MRI suggestion; choose a supported MRI manually or use ordinary import.".into());
    }
    if scan.postop_ct.is_none() {
        scan.warnings
            .push("No metal-positive CT suggestion; review CT volumes manually.".into());
    }
    Ok(scan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dicom_core::{DataElement, PrimitiveValue, VR};
    use dicom_object::{FileMetaTableBuilder, InMemDicomObject};

    fn write_dicom(
        path: &Path,
        modality: &str,
        uid: &str,
        index: usize,
        description: &str,
        date: &str,
        metal: bool,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut obj = InMemDicomObject::new_empty();
        for (tag, vr, value) in [
            (
                tags::SOP_CLASS_UID,
                VR::UI,
                "1.2.840.10008.5.1.4.1.1.2".to_owned(),
            ),
            (
                tags::SOP_INSTANCE_UID,
                VR::UI,
                format!("{uid}.{}", index + 1),
            ),
            (tags::SERIES_INSTANCE_UID, VR::UI, uid.into()),
            (tags::MODALITY, VR::CS, modality.into()),
            (tags::SERIES_DESCRIPTION, VR::LO, description.into()),
            (tags::STUDY_DATE, VR::DA, date.into()),
            (tags::INSTANCE_NUMBER, VR::IS, (index + 1).to_string()),
            (
                tags::IMAGE_POSITION_PATIENT,
                VR::DS,
                format!("0\\0\\{}", index as f64),
            ),
            (
                tags::IMAGE_ORIENTATION_PATIENT,
                VR::DS,
                "1\\0\\0\\0\\1\\0".into(),
            ),
            (tags::PIXEL_SPACING, VR::DS, "1\\1".into()),
            (tags::SLICE_THICKNESS, VR::DS, "1".into()),
            (
                tags::PHOTOMETRIC_INTERPRETATION,
                VR::CS,
                "MONOCHROME2".into(),
            ),
            (tags::RESCALE_SLOPE, VR::DS, "2".into()),
            (tags::RESCALE_INTERCEPT, VR::DS, "-1024".into()),
        ] {
            obj.put(DataElement::new(tag, vr, value));
        }
        for (tag, value) in [
            (tags::ROWS, 64u16),
            (tags::COLUMNS, 64),
            (tags::SAMPLES_PER_PIXEL, 1),
            (tags::BITS_ALLOCATED, 16),
            (tags::BITS_STORED, 16),
            (tags::HIGH_BIT, 15),
            (tags::PIXEL_REPRESENTATION, 0),
        ] {
            obj.put(DataElement::new(tag, VR::US, PrimitiveValue::from(value)));
        }
        let mut pixels = vec![1500u16; 64 * 64];
        if metal {
            pixels[32 * 64 + 32] = 2000;
            pixels[32 * 64 + 33] = 2000;
        }
        obj.put(DataElement::new(
            tags::PIXEL_DATA,
            VR::OW,
            PrimitiveValue::U16(pixels.into()),
        ));
        let obj = obj.with_meta(
            FileMetaTableBuilder::new()
                .transfer_syntax("1.2.840.10008.1.2.1")
                .media_storage_sop_class_uid("1.2.840.10008.5.1.4.1.1.2"),
        )?;
        obj.write_to_file(path)?;
        Ok(())
    }

    #[test]
    fn calendar_dates() {
        assert!(!valid_date("00000000"));
        assert!(!valid_date("20251399"));
        assert!(!valid_date("20250229"));
        assert!(valid_date("20240229"));
        assert!(!valid_date("19000229"));
        assert!(valid_date("20000229"));
    }

    #[test]
    fn synthetic_dicom_scan_groups_deduplicates_and_applies_rescale_once(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        for (folder, m, uid, description, date, metal) in [
            (
                "CT/export-002",
                "CT",
                "1.2.3.1",
                "Head thin",
                "20250102",
                true,
            ),
            (
                "MRI/export-010",
                "MR",
                "1.2.3.2",
                "T1 mprage",
                "20250101",
                false,
            ),
            (
                "MRI/export-001",
                "MR",
                "1.2.3.3",
                "T1 mprage",
                "20250101",
                false,
            ),
        ] {
            let dir = temp.path().join(folder);
            std::fs::create_dir_all(&dir)?;
            for i in 0..20 {
                write_dicom(
                    &dir.join(format!("image{i:03}")),
                    m,
                    uid,
                    i,
                    description,
                    date,
                    metal,
                )?;
            }
            std::fs::copy(dir.join("image000"), dir.join("duplicate.dcm"))?;
        }
        std::fs::create_dir_all(temp.path().join("CT/export-002/JRE"))?;
        std::fs::write(temp.path().join("CT/export-002/JRE/ignored"), b"viewer")?;
        let scan = scan_patient(temp.path(), &AtomicBool::new(false), |_| {})?;
        assert_eq!(scan.candidates.len(), 3);
        assert!(scan.candidates.iter().all(|c| c.slice_count == 20));
        let m = &scan.candidates[scan.preop_mri.ok_or("missing MRI suggestion")?];
        assert_eq!(m.session_number, 10);
        assert_eq!(
            m.preview.as_ref().ok_or("missing preview")?.pixels[0],
            1976.
        );
        let ct = &scan.candidates[scan.postop_ct.ok_or("missing CT suggestion")?];
        assert_eq!(ct.evidence, ElectrodeEvidence::Detected);
        assert_eq!(
            ct.preview.as_ref().ok_or("missing preview")?.pixels[32 * 64 + 32],
            2976.
        );
        Ok(())
    }
    #[test]
    fn numeric_sessions() {
        assert_eq!(session_number("export-001"), Some(1));
        assert_eq!(session_number("010"), Some(10));
        assert_eq!(session_number("2"), Some(2));
        assert_eq!(session_number("0"), None);
        assert_eq!(session_number("viewer10"), None);
        assert_eq!(modality("mr"), Some(Modality::Mr));
        assert_eq!(modality("MRI"), Some(Modality::Mr));
    }

    fn acquisition(tr: f64, te: f64, ti: Option<f64>, family: &str, dim: &str) -> Acquisition {
        Acquisition {
            tr: Some(tr),
            te: Some(te),
            ti,
            family: family.into(),
            dimensionality: dim.into(),
            ..Acquisition::default()
        }
    }
    #[test]
    fn dicom_zero_b_value_alone_does_not_exclude_structural_mri(
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut obj = InMemDicomObject::new_empty();
        for (tag, vr, val) in [
            (tags::REPETITION_TIME, VR::DS, "10.6188"),
            (tags::ECHO_TIME, VR::DS, "6.005"),
            (tags::FLIP_ANGLE, VR::DS, "8"),
            (tags::SCANNING_SEQUENCE, VR::CS, "GR"),
            (tags::SEQUENCE_VARIANT, VR::CS, "MP"),
            (tags::MR_ACQUISITION_TYPE, VR::CS, "3D"),
        ] {
            obj.put(DataElement::new(tag, vr, val));
        }
        obj.put(DataElement::new(
            tags::DIFFUSION_B_VALUE,
            VR::FD,
            PrimitiveValue::from(0.0f64),
        ));
        let mut obj = obj.with_meta(
            FileMetaTableBuilder::new()
                .transfer_syntax("1.2.840.10008.1.2.1")
                .media_storage_sop_class_uid("1.2.840.10008.5.1.4.1.1.4")
                .media_storage_sop_instance_uid("1.2.3.99"),
        )?;
        let a = Acquisition::read(&obj);
        assert!(!a.diffusion);
        assert_eq!(a.b_value, Some(0.));
        assert_eq!(infer_suffix("", &a).0.as_deref(), Some("T1w"));
        obj.put(DataElement::new(tags::SCANNING_SEQUENCE, VR::CS, "EP"));
        assert_eq!(infer_suffix("", &Acquisition::read(&obj)).0, None);
        obj.put(DataElement::new(tags::SCANNING_SEQUENCE, VR::CS, "GR"));
        obj.put(DataElement::new(
            tags::ARTERIAL_SPIN_LABELING_CONTRAST,
            VR::CS,
            "PSEUDOCONTINUOUS",
        ));
        assert_eq!(infer_suffix("", &Acquisition::read(&obj)).0, None);
        obj.remove_element(tags::ARTERIAL_SPIN_LABELING_CONTRAST);
        obj.put(DataElement::new(
            tags::COMPLEX_IMAGE_COMPONENT,
            VR::CS,
            "PHASE",
        ));
        assert_eq!(infer_suffix("", &Acquisition::read(&obj)).0, None);
        Ok(())
    }
    #[test]
    fn anatomical_parameter_families_use_milliseconds() {
        for (a, expected) in [
            (
                acquisition(9000., 90., Some(2500.), "IR\\SE", "2D"),
                "FLAIR",
            ),
            (acquisition(2300., 3., Some(900.), "IR\\GR", "3D"), "T1w"),
            (acquisition(5000., 90., None, "SE", "2D"), "T2w"),
            (acquisition(4000., 15., None, "SE", "2D"), "PDw"),
            (acquisition(500., 12., None, "SE", "2D"), "T1w"),
            (acquisition(8., 3., None, "GR", "3D"), "T1w"),
        ] {
            let (s, why) = infer_suffix("", &a);
            assert_eq!(s.as_deref(), Some(expected));
            assert!(why.contains("ms"));
            assert!(why.contains("guess"));
        }
    }
    #[test]
    fn parameter_ambiguity_and_nonfinite_values_remain_unknown() {
        for a in [
            acquisition(0.008, 0.003, None, "GR", "3D"),
            acquisition(5000., 90., None, "GR", "2D"),
            acquisition(4000., 15., None, "", "2D"),
            acquisition(8., 3., None, "GR", "2D"),
            acquisition(f64::NAN, 3., None, "GR", "3D"),
            acquisition(8., f64::INFINITY, None, "GR", "3D"),
            acquisition(8., 3., Some(f64::NAN), "GR", "3D"),
            acquisition(8., 30., None, "GR", "3D"),
            acquisition(60001., 90., None, "SE", "2D"),
            acquisition(2000., 1001., None, "SE", "2D"),
            acquisition(5000., 90., Some(6000.), "IR\\SE", "2D"),
            acquisition(50000., 90., Some(21000.), "IR\\SE", "2D"),
            Acquisition::default(),
        ] {
            assert_eq!(infer_suffix("", &a).0, None);
        }
    }
    #[test]
    fn excluded_acquisitions_win_over_names_and_parameters() {
        let a = acquisition(8., 3., None, "GR", "3D");
        for name in [
            "t1_dwi",
            "T1_ADC",
            "t2_perfusion",
            "t1_asl",
            "t1_bold",
            "t1_fieldmap",
            "t1 phase",
            "t1_phase",
            "t1_phasediff",
            "t2_swi",
            "t1 susceptibility",
        ] {
            assert_eq!(infer_suffix(&name.to_lowercase(), &a).0, None);
        }
        let mut d = a.clone();
        d.diffusion = true;
        assert_eq!(infer_suffix("mprage", &d).0, None);
        d.diffusion = false;
        d.phase = true;
        assert_eq!(infer_suffix("t1", &d).0, None);
        d.phase = false;
        d.family = "EP".into();
        assert_eq!(infer_suffix("t1", &d).0, None);
        d.family = "GR".into();
        d.perfusion_or_functional = true;
        assert_eq!(infer_suffix("t1", &d).0, None);
        d.perfusion_or_functional = false;
        d.variant = "SS".into();
        assert_eq!(infer_suffix("", &d).0, None);
        assert_eq!(infer_suffix("t2star", &a).0.as_deref(), Some("T2starw"));
        assert_eq!(infer_suffix("t2 flair", &a).0.as_deref(), Some("FLAIR"));
        assert_eq!(infer_suffix("fgatir", &a).0.as_deref(), Some("FGATIR"));
    }
    fn candidate(n: u32, uid: &str, m: Modality, e: ElectrodeEvidence) -> Candidate {
        Candidate {
            session_number: n,
            session_path: PathBuf::new(),
            series_uid: uid.into(),
            modality: m,
            description: String::new(),
            study_date: None,
            slice_count: 100,
            spacing_mm: [1.; 3],
            evidence: e,
            reason: String::new(),
            eligible: true,
            suffix: Some(if m == Modality::Mr { "T1w" } else { "CT" }.into()),
            suffix_reason: String::new(),
            preview: None,
            slices: Vec::new(),
            quality: 1.,
            explicit_preop: m == Modality::Mr,
            explicit_postop: false,
        }
    }
    #[test]
    fn oldest_wins_over_quality_and_uid_breaks_ties() {
        let mut old = candidate(10, "z", Modality::Mr, ElectrodeEvidence::LikelyAbsent);
        old.quality = 0.;
        let mut newer = candidate(2, "a", Modality::Mr, ElectrodeEvidence::LikelyAbsent);
        newer.quality = 100.;
        let mut scan = Scan {
            candidates: vec![
                newer,
                old,
                candidate(10, "a", Modality::Mr, ElectrodeEvidence::LikelyAbsent),
                candidate(1, "x", Modality::Ct, ElectrodeEvidence::Detected),
            ],
            ..Scan::default()
        };
        recommend(&mut scan);
        assert_eq!(scan.preop_mri, Some(2));
        assert_eq!(scan.postop_ct, Some(3));
    }
    #[test]
    fn unknown_is_not_negative_and_dates_override_preop_keyword() {
        let mut m = candidate(3, "m", Modality::Mr, ElectrodeEvidence::Unknown);
        m.study_date = Some("20250103".into());
        let mut ct = candidate(2, "ct", Modality::Ct, ElectrodeEvidence::Detected);
        ct.study_date = Some("20250102".into());
        let mut scan = Scan {
            candidates: vec![m, ct],
            ..Scan::default()
        };
        recommend(&mut scan);
        assert_eq!(scan.preop_mri, None);
        assert_eq!(scan.candidates[0].evidence, ElectrodeEvidence::Unknown);
        scan.candidates[0].study_date = Some("20250101".into());
        recommend(&mut scan);
        assert_eq!(scan.preop_mri, Some(0));
        scan.candidates[0].explicit_postop = true;
        recommend(&mut scan);
        assert_eq!(scan.preop_mri, None);
    }
    #[test]
    fn cancellation_and_traversal() {
        let cancel = AtomicBool::new(true);
        assert!(matches!(
            scan_patient(Path::new("/tmp"), &cancel, |_| {}),
            Err(Error::Cancelled)
        ));
        cancel.store(false, Ordering::Relaxed);
        assert!(matches!(
            scan_patient(Path::new("/tmp/../tmp"), &cancel, |_| {}),
            Err(Error::Traversal(_))
        ));
    }
    #[test]
    fn ignored_viewer_and_symlink_do_not_form_sessions() -> Result<(), Box<dyn std::error::Error>> {
        let temp = tempfile::tempdir()?;
        std::fs::create_dir_all(temp.path().join("CT/viewer1"))?;
        assert!(matches!(
            scan_patient(temp.path(), &AtomicBool::new(false), |_| {}),
            Err(Error::NoSessions)
        ));
        Ok(())
    }
}
