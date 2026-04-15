#![allow(clippy::disallowed_methods, reason = "tooling is exempt")]

use anyhow::{Context as _, Result, bail};
use cargo_metadata::semver;
use clap::{Parser, ValueEnum};
use serde::Serialize;
use std::process::Command;

const RELEASE_API_URL: &str = "https://cloud.zed.dev/releases";
const RELEASE_CHANNEL_PATH: &str = "crates/zed/RELEASE_CHANNEL";

#[derive(Clone, ValueEnum)]
pub enum BumpType {
    Major,
    Minor,
    Patch,
}

impl std::fmt::Display for BumpType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BumpType::Major => write!(f, "major"),
            BumpType::Minor => write!(f, "minor"),
            BumpType::Patch => write!(f, "patch"),
        }
    }
}

#[derive(Clone, ValueEnum)]
pub enum PatchChannel {
    Preview,
    Stable,
    Both,
}

#[derive(Parser)]
pub struct PlanArgs {
    /// Which version component to bump.
    #[arg(long, value_enum)]
    pub bump_type: BumpType,

    /// For patch bumps: which channel branches to target.
    #[arg(long, value_enum, default_value = "both")]
    pub patch_channel: PatchChannel,
}

#[derive(Serialize)]
struct MatrixEntry {
    checkout_ref: String,
    target_branch: String,
    /// Cargo set-version bump type (e.g. "minor", "patch"), or empty to skip.
    bump: String,
    /// Channel to write to RELEASE_CHANNEL (e.g. "preview", "stable"), or empty to skip.
    new_channel: String,
}

/// Queries the Zed release API to find the version branch for a given channel.
///
/// Returns a branch name like `v0.232.x`.
fn resolve_channel_branch(channel: &str) -> Result<String> {
    let url = format!("{RELEASE_API_URL}/{channel}/latest/asset?asset=zed&os=macos&arch=aarch64");

    let output = Command::new("curl")
        .args(["-fsSL", &url])
        .output()
        .with_context(|| format!("Failed to fetch latest {channel} release from API"))?;

    if !output.status.success() {
        bail!(
            "API request for {channel} channel failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    #[derive(serde::Deserialize)]
    struct ReleaseResponse {
        version: semver::Version,
    }

    let response: ReleaseResponse = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("Failed to parse {channel} release API response"))?;

    Ok(format!(
        "v{major}.{minor}.x",
        major = response.version.major,
        minor = response.version.minor
    ))
}

pub fn run_plan(args: PlanArgs) -> Result<()> {
    let preview_branch = resolve_channel_branch("preview")?;
    let stable_branch = resolve_channel_branch("stable")?;
    eprintln!("Resolved preview branch: {preview_branch}");
    eprintln!("Resolved stable branch:  {stable_branch}");

    let metadata = cargo_metadata::MetadataCommand::new()
        .no_deps()
        .exec()
        .context("Failed to run cargo metadata")?;

    let zed_package = metadata
        .packages
        .iter()
        .find(|p| p.name == "zed")
        .ok_or_else(|| anyhow::anyhow!("zed package not found in workspace"))?;

    let version = &zed_package.version;
    eprintln!("Version on main: {version}");

    let channel =
        std::fs::read_to_string(RELEASE_CHANNEL_PATH).context("Failed to read RELEASE_CHANNEL")?;
    let channel = channel.trim();

    if channel != "dev" && channel != "nightly" {
        bail!("RELEASE_CHANNEL on main must be 'dev' or 'nightly', found: '{channel}'");
    }

    let matrix: Vec<MatrixEntry> = match args.bump_type {
        BumpType::Patch => {
            let mut entries = Vec::new();
            if matches!(
                args.patch_channel,
                PatchChannel::Preview | PatchChannel::Both
            ) {
                entries.push(MatrixEntry {
                    checkout_ref: preview_branch.clone(),
                    target_branch: preview_branch,
                    bump: "patch".into(),
                    new_channel: String::new(),
                });
            }
            if matches!(
                args.patch_channel,
                PatchChannel::Stable | PatchChannel::Both
            ) {
                entries.push(MatrixEntry {
                    checkout_ref: stable_branch.clone(),
                    target_branch: stable_branch,
                    bump: "patch".into(),
                    new_channel: String::new(),
                });
            }
            entries
        }
        BumpType::Major | BumpType::Minor => {
            let new_preview_branch = format!("v{}.{}.x", version.major, version.minor);
            vec![
                MatrixEntry {
                    checkout_ref: "main".into(),
                    target_branch: "main".into(),
                    bump: args.bump_type.to_string(),
                    new_channel: String::new(),
                },
                MatrixEntry {
                    checkout_ref: "main".into(),
                    target_branch: new_preview_branch,
                    bump: String::new(),
                    new_channel: "preview".into(),
                },
                MatrixEntry {
                    checkout_ref: preview_branch.clone(),
                    target_branch: preview_branch,
                    bump: String::new(),
                    new_channel: "stable".into(),
                },
            ]
        }
    };

    println!("{}", serde_json::to_string(&matrix)?);
    Ok(())
}
