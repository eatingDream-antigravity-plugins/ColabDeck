//! [DP7.1] GGUF quantisation grouping and GPU VRAM fit planner.
//!
//! Pure arithmetic module powering real-time UI deployment previews
//! (e.g. `G4 96 GB -> Q8_0`, `A100 40 GB -> Q6_K (q8_0 KV)`, `A100 80 GB -> Q8_0`).

use crate::settings::Accelerator;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const RESERVE_MIB: f64 = 1024.0;
pub const COMPUTE_MIB: f64 = 512.0;
pub const COMPUTE_FRACTION: f64 = 0.05;
pub const BUDGET_FRACTION: f64 = 0.97;
pub const FALLBACK_KV_BYTES_PER_TOKEN: u64 = 128 * 1024;
pub const FALLBACK_CTX: u32 = 32_768;
pub const MIN_AUTO_CTX: u32 = 8_192;

pub fn vram_choices_gib(accelerator: Accelerator) -> &'static [u32] {
    match accelerator {
        Accelerator::T4 => &[15],
        Accelerator::L4 => &[22],
        Accelerator::A100 => &[40, 80],
        Accelerator::H100 => &[80],
        Accelerator::G4 => &[96],
        Accelerator::None | Accelerator::TpuV5e1 | Accelerator::TpuV6e1 => &[],
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GgufFile {
    pub path: String,
    pub size: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GgufVariant {
    pub quant: String,
    pub files: Vec<String>,
    pub size: u64,
}

impl GgufVariant {
    pub fn is_full_precision(&self) -> bool {
        matches!(self.quant.as_str(), "F16" | "BF16" | "F32")
    }

    pub fn is_low_bit(&self) -> bool {
        let q = self.quant.trim_start_matches("UD-");
        ["Q3", "Q2", "Q1", "IQ3", "IQ2", "IQ1", "TQ"]
            .iter()
            .any(|p| q.starts_with(p))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GgufListing {
    pub variants: Vec<GgufVariant>,
    pub mmproj: Vec<GgufFile>,
}

impl GgufListing {
    pub fn preferred_mmproj(&self) -> Option<&GgufFile> {
        let rank = |p: &str| {
            let l = p.to_ascii_lowercase();
            if l.contains("-f16") || l.contains("_f16") {
                0
            } else if l.contains("bf16") {
                1
            } else {
                2
            }
        };
        self.mmproj.iter().min_by_key(|f| (rank(&f.path), f.size))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FitChoice {
    pub vram_gib: u32,
    pub quant: String,
    pub kv: String,
    pub ctx: u32,
    pub estimate_mib: u32,
    pub budget_mib: u32,
    pub fits: bool,
}

fn is_quant_token(token: &str) -> bool {
    let u = token.to_ascii_uppercase();
    if u.is_empty() || !u.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return false;
    }
    if matches!(u.as_str(), "F16" | "F32" | "BF16") || u.starts_with("MXFP4") {
        return true;
    }
    for prefix in ["IQ", "TQ", "Q"] {
        if let Some(rest) = u.strip_prefix(prefix) {
            return rest.bytes().next().is_some_and(|b| b.is_ascii_digit());
        }
    }
    false
}

fn strip_shard_suffix(stem: &str) -> &str {
    if stem.len() >= 15 {
        let tail = &stem[stem.len() - 15..];
        let b = tail.as_bytes();
        if b[0] == b'-'
            && b[1..6].iter().all(u8::is_ascii_digit)
            && &tail[6..10] == "-of-"
            && b[10..15].iter().all(u8::is_ascii_digit)
        {
            return &stem[..stem.len() - 15];
        }
    }
    stem
}

pub fn quant_label(path: &str) -> Option<String> {
    let base = path.rsplit('/').next().unwrap_or(path);
    if !base.to_ascii_lowercase().ends_with(".gguf") {
        return None;
    }
    let stem = strip_shard_suffix(&base[..base.len() - 5]);
    let tokens: Vec<&str> = stem.split(['-', '.']).collect();
    for i in (0..tokens.len()).rev() {
        if is_quant_token(tokens[i]) {
            let label = tokens[i].to_ascii_uppercase();
            if i > 0 && tokens[i - 1].eq_ignore_ascii_case("ud") {
                return Some(format!("UD-{label}"));
            }
            return Some(label);
        }
    }
    None
}

pub fn group_gguf_files(files: Vec<GgufFile>) -> GgufListing {
    let mut by_label: BTreeMap<String, GgufVariant> = BTreeMap::new();
    let mut mmproj = Vec::new();
    for f in files {
        let base = f.path.rsplit('/').next().unwrap_or(&f.path);
        if !base.to_ascii_lowercase().ends_with(".gguf") {
            continue;
        }
        if base.to_ascii_lowercase().contains("mmproj") {
            mmproj.push(f);
            continue;
        }
        let Some(label) = quant_label(&f.path) else {
            continue;
        };
        let entry = by_label.entry(label.clone()).or_insert(GgufVariant {
            quant: label,
            files: Vec::new(),
            size: 0,
        });
        entry.files.push(f.path);
        entry.size = entry.size.saturating_add(f.size);
    }
    let mut variants: Vec<GgufVariant> = by_label
        .into_values()
        .map(|mut v| {
            v.files.sort();
            v
        })
        .collect();
    variants.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.quant.cmp(&b.quant)));
    GgufListing { variants, mmproj }
}

fn kv_factor(kv: &str) -> f64 {
    match kv {
        "q8_0" => 34.0 / 64.0,
        "q4_0" => 18.0 / 64.0,
        _ => 1.0,
    }
}

pub fn estimate_mib(weights: u64, mmproj: u64, ctx: u32, kv: &str) -> f64 {
    let mib = 1024.0 * 1024.0;
    let w = weights as f64 / mib;
    let mm = mmproj as f64 / mib;
    let kv_mib = (FALLBACK_KV_BYTES_PER_TOKEN as f64) * kv_factor(kv) * (ctx as f64) / mib;
    w + mm + kv_mib + RESERVE_MIB + COMPUTE_MIB + COMPUTE_FRACTION * w
}

pub fn fit_for_vram(
    listing: &GgufListing,
    vram_gib: u32,
    requested_quant: &str,
    requested_ctx: u32,
) -> FitChoice {
    let vram_mib = (vram_gib as f64) * 1024.0;
    let budget = vram_mib * BUDGET_FRACTION;
    let mm_bytes = listing.preferred_mmproj().map(|m| m.size).unwrap_or(0);

    let mut ctx_ladder = Vec::new();
    if requested_ctx > 0 {
        ctx_ladder.push(requested_ctx);
    } else {
        let mut c = FALLBACK_CTX;
        ctx_ladder.push(c);
        while c / 2 >= MIN_AUTO_CTX {
            c /= 2;
            ctx_ladder.push(c);
        }
    }

    let try_variant = |v: &GgufVariant, ctx: u32| -> Option<FitChoice> {
        for kv in ["f16", "q8_0"] {
            let est = estimate_mib(v.size, mm_bytes, ctx, kv);
            if est <= budget {
                return Some(FitChoice {
                    vram_gib,
                    quant: v.quant.clone(),
                    kv: kv.to_owned(),
                    ctx,
                    estimate_mib: est.ceil() as u32,
                    budget_mib: budget as u32,
                    fits: true,
                });
            }
        }
        None
    };

    if !requested_quant.eq_ignore_ascii_case("auto") && !requested_quant.is_empty() {
        if let Some(v) = listing
            .variants
            .iter()
            .find(|x| x.quant.eq_ignore_ascii_case(requested_quant))
        {
            for &ctx in &ctx_ladder {
                if let Some(choice) = try_variant(v, ctx) {
                    return choice;
                }
            }
        }
        return FitChoice {
            vram_gib,
            quant: requested_quant.to_owned(),
            kv: "q8_0".into(),
            ctx: ctx_ladder[0],
            estimate_mib: 0,
            budget_mib: budget as u32,
            fits: false,
        };
    }

    let quantised: Vec<&GgufVariant> = listing
        .variants
        .iter()
        .filter(|v| !v.is_full_precision())
        .collect();
    let pool: Vec<&GgufVariant> = if quantised.is_empty() {
        listing.variants.iter().collect()
    } else {
        quantised
    };
    let normal: Vec<&GgufVariant> = pool.iter().copied().filter(|v| !v.is_low_bit()).collect();

    for tier in [&normal, &pool] {
        for &ctx in &ctx_ladder {
            for &v in tier {
                if let Some(choice) = try_variant(v, ctx) {
                    return choice;
                }
            }
        }
    }

    FitChoice {
        vram_gib,
        quant: "none".into(),
        kv: "q8_0".into(),
        ctx: MIN_AUTO_CTX,
        estimate_mib: 0,
        budget_mib: budget as u32,
        fits: false,
    }
}

pub fn preview_for_accelerator(
    listing: &GgufListing,
    accelerator: Accelerator,
    requested_quant: &str,
    requested_ctx: u32,
) -> Vec<FitChoice> {
    vram_choices_gib(accelerator)
        .iter()
        .map(|&gib| fit_for_vram(listing, gib, requested_quant, requested_ctx))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gguf_grouping_and_a100_40_80_and_g4_fit() {
        let gb = 1024 * 1024 * 1024u64;
        let listing = group_gguf_files(vec![
            GgufFile {
                path: "model-Q8_0-00001-of-00002.gguf".into(),
                size: 15 * gb,
            },
            GgufFile {
                path: "model-Q8_0-00002-of-00002.gguf".into(),
                size: 15 * gb,
            },
            GgufFile {
                path: "model-Q6_K.gguf".into(),
                size: 22 * gb,
            },
            GgufFile {
                path: "model-Q4_K_M.gguf".into(),
                size: 16 * gb,
            },
            GgufFile {
                path: "mmproj-model-f16.gguf".into(),
                size: gb,
            },
        ]);
        assert_eq!(listing.variants.len(), 3);
        assert_eq!(listing.variants[0].quant, "Q8_0");
        assert_eq!(listing.variants[0].files.len(), 2);

        let a100 = preview_for_accelerator(&listing, Accelerator::A100, "auto", 0);
        assert_eq!(a100.len(), 2);
        assert_eq!(a100[0].vram_gib, 40);
        assert!(a100[0].fits);
        assert_eq!(a100[1].vram_gib, 80);
        assert_eq!(a100[1].quant, "Q8_0");

        let g4 = preview_for_accelerator(&listing, Accelerator::G4, "auto", 0);
        assert_eq!(g4[0].vram_gib, 96);
        assert_eq!(g4[0].quant, "Q8_0");
    }
}
