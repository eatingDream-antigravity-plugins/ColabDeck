//! [DP7.2] Hugging Face GGUF repository search and file-tree inspection.
//!
//! Uses `Host::https_get` (SSRF-safe, bearer token via stdin) in real mode,
//! and returns a curated deterministic catalog in Mock mode.

use crate::error::{Error, Result};
use crate::host::Host;
use crate::quant::{group_gguf_files, GgufFile, GgufListing};
use crate::util::encode_query;
use serde::{Deserialize, Serialize};

pub const HF_BASE: &str = "https://huggingface.co";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HfModel {
    pub id: String,
    pub downloads: u64,
    pub likes: u64,
    pub gated: bool,
}

pub fn is_valid_repo_id(repo: &str) -> bool {
    let Some((owner, name)) = repo.split_once('/') else {
        return false;
    };
    let ok_seg = |s: &str| {
        !s.is_empty()
            && s.len() <= 96
            && s != "."
            && s != ".."
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    };
    !name.contains('/') && ok_seg(owner) && ok_seg(name)
}

pub fn search_gguf_models(
    host: &Host,
    query: &str,
    limit: u32,
    token: Option<&str>,
    is_mock: bool,
) -> Result<Vec<HfModel>> {
    let q = query.trim();
    if is_mock {
        return Ok(mock_search(q));
    }
    let url = format!(
        "{HF_BASE}/api/models?search={}&filter=gguf&sort=downloads&direction=-1&limit={}",
        encode_query(q),
        limit.clamp(1, 30)
    );
    let body = host.https_get(&url, token, 15)?;
    parse_search(&body)
}

pub fn list_gguf_files(
    host: &Host,
    repo: &str,
    token: Option<&str>,
    is_mock: bool,
) -> Result<GgufListing> {
    let repo = repo.trim();
    if !is_valid_repo_id(repo) {
        return Err(Error::invalid(
            "repo",
            "expected Hugging Face repo id `owner/name`",
        ));
    }
    if is_mock {
        return Ok(mock_listing(repo));
    }
    let url = format!("{HF_BASE}/api/models/{repo}/tree/main?recursive=true");
    let body = host.https_get(&url, token, 15)?;
    let files = parse_tree(&body)?;
    Ok(group_gguf_files(files))
}

fn parse_search(body: &[u8]) -> Result<Vec<HfModel>> {
    let arr: Vec<serde_json::Value> = serde_json::from_slice(body)?;
    Ok(arr
        .iter()
        .filter_map(|m| {
            let id = m
                .get("id")
                .or_else(|| m.get("modelId"))?
                .as_str()?
                .to_owned();
            Some(HfModel {
                id,
                downloads: m.get("downloads").and_then(|d| d.as_u64()).unwrap_or(0),
                likes: m.get("likes").and_then(|d| d.as_u64()).unwrap_or(0),
                gated: m
                    .get("gated")
                    .is_some_and(|g| g.as_bool().unwrap_or(g.is_string())),
            })
        })
        .collect())
}

fn parse_tree(body: &[u8]) -> Result<Vec<GgufFile>> {
    let arr: Vec<serde_json::Value> = serde_json::from_slice(body)?;
    Ok(arr
        .iter()
        .filter(|f| f.get("type").and_then(|t| t.as_str()) == Some("file"))
        .filter_map(|f| {
            let path = f.get("path")?.as_str()?.to_owned();
            if !path.to_ascii_lowercase().ends_with(".gguf") {
                return None;
            }
            let size = f
                .get("lfs")
                .and_then(|l| l.get("size"))
                .and_then(|s| s.as_u64())
                .or_else(|| f.get("size").and_then(|s| s.as_u64()))
                .unwrap_or(0);
            Some(GgufFile { path, size })
        })
        .collect())
}

fn mock_search(query: &str) -> Vec<HfModel> {
    let catalog = vec![
        HfModel {
            id: "unsloth/Qwen3-32B-GGUF".into(),
            downloads: 412_000,
            likes: 1_820,
            gated: false,
        },
        HfModel {
            id: "unsloth/Qwen3-8B-GGUF".into(),
            downloads: 295_000,
            likes: 1_140,
            gated: false,
        },
        HfModel {
            id: "bartowski/Llama-3.3-70B-Instruct-GGUF".into(),
            downloads: 188_000,
            likes: 960,
            gated: false,
        },
        HfModel {
            id: "unsloth/gemma-3-27b-it-GGUF".into(),
            downloads: 152_000,
            likes: 830,
            gated: false,
        },
    ];
    if query.is_empty() {
        return catalog;
    }
    let low = query.to_ascii_lowercase();
    let matched: Vec<HfModel> = catalog
        .into_iter()
        .filter(|m| m.id.to_ascii_lowercase().contains(&low))
        .collect();
    if matched.is_empty() {
        vec![HfModel {
            id: format!("unsloth/{}-GGUF", query.replace('/', "-")),
            downloads: 12_400,
            likes: 95,
            gated: false,
        }]
    } else {
        matched
    }
}

fn mock_listing(repo: &str) -> GgufListing {
    let gb = 1024 * 1024 * 1024u64;
    let scale = if repo.contains("70B") {
        2
    } else if repo.contains("8B") {
        0
    } else {
        1
    };
    let (q8, q6, q4) = match scale {
        0 => (9 * gb, 7 * gb, 5 * gb),
        2 => (72 * gb, 56 * gb, 41 * gb),
        _ => (34 * gb, 26 * gb, 19 * gb),
    };
    group_gguf_files(vec![
        GgufFile {
            path: "model-Q8_0.gguf".into(),
            size: q8,
        },
        GgufFile {
            path: "model-Q6_K.gguf".into(),
            size: q6,
        },
        GgufFile {
            path: "model-Q4_K_M.gguf".into(),
            size: q4,
        },
    ])
}
