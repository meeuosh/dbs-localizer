// SPDX-FileCopyrightText: 2026 Wyss Center for Bio and Neuroengineering
// SPDX-License-Identifier: GPL-3.0-only

//! Standalone Zurich triage & anonymized diagnostics CLI (`dbsloc-zurich`).

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use dbsloc_zurich::{build_debug_report, canonical_path, scan_patient};

fn usage() -> &'static str {
    "Usage:
  dbsloc-zurich scan <patient-dir>
  dbsloc-zurich debug-case <patient-dir> [--case LABEL] [--out PATH]"
}

fn main() {
    if let Err(err) = run() {
        eprintln!("dbsloc-zurich: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() || args[0] == "-h" || args[0] == "--help" {
        println!("{}", usage());
        return Ok(());
    }

    match args[0].as_str() {
        "scan" => {
            let dir = args.get(1).ok_or_else(|| usage().to_owned())?;
            let root = canonical_path(&PathBuf::from(dir))?;
            let scan = scan_patient(&root, &AtomicBool::new(false), |_| {})?;
            println!("Candidates scanned: {}", scan.candidates.len());
            match scan.postop_ct {
                Some(idx) => {
                    let c = &scan.candidates[idx];
                    println!(
                        "Chosen CT : CT/export-{:03} | slices={} | spacing={:.2}x{:.2}x{:.2}mm | kernel={:?} | orient={:?} | {}",
                        c.session_number,
                        c.slice_count,
                        c.spacing_mm[0],
                        c.spacing_mm[1],
                        c.spacing_mm[2],
                        c.ct_kernel,
                        c.orientation,
                        c.reason
                    );
                }
                None => println!("Chosen CT : NONE"),
            }
            if scan.preop_mris.is_empty() {
                println!("Chosen MR : NONE");
            } else {
                println!(
                    "Chosen MR study: {} anatomical series",
                    scan.preop_mris.len()
                );
                for &idx in &scan.preop_mris {
                    let m = &scan.candidates[idx];
                    println!(
                        "  - MR/export-{:03} | type={} ({:?}) | orient={:?} | slices={} | spacing={:.2}x{:.2}x{:.2}mm",
                        m.session_number,
                        m.suffix.as_deref().unwrap_or("Unknown"),
                        m.contrast_source,
                        m.orientation,
                        m.slice_count,
                        m.spacing_mm[0],
                        m.spacing_mm[1],
                        m.spacing_mm[2],
                    );
                }
            }
            for w in &scan.warnings {
                eprintln!("warning: {w}");
            }
            Ok(())
        }
        "debug-case" => {
            let dir = args.get(1).ok_or_else(|| usage().to_owned())?;
            let mut case_label = "CASE-01".to_owned();
            let mut out_path: Option<PathBuf> = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "--case" => {
                        i += 1;
                        case_label = args.get(i).ok_or("--case needs a value")?.clone();
                    }
                    "--out" => {
                        i += 1;
                        out_path = Some(PathBuf::from(args.get(i).ok_or("--out needs a path")?));
                    }
                    other => return Err(format!("unknown option: {other}\n{}", usage()).into()),
                }
                i += 1;
            }
            let root = canonical_path(&PathBuf::from(dir))?;
            let scan = scan_patient(&root, &AtomicBool::new(false), |_| {})?;
            let report = build_debug_report(&scan, &case_label);
            let json = serde_json::to_string_pretty(&report)?;
            if let Some(path) = out_path {
                std::fs::write(&path, format!("{json}\n"))?;
                eprintln!("Wrote anonymized debug report to {}", path.display());
            } else {
                println!("{json}");
            }
            Ok(())
        }
        other => Err(format!("unknown subcommand: {other}\n{}", usage()).into()),
    }
}
