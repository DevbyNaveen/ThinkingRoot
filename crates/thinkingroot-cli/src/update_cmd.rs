use anyhow::Context;
use console::style;
use serde::Deserialize;

const RELEASES_REPO: &str = "DevbyNaveen/releases";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
}

pub async fn run_update() -> anyhow::Result<()> {
    println!();
    println!("  {} Checking for updates...", style("→").cyan());

    let client = reqwest::Client::builder()
        .user_agent(format!("root/{CURRENT_VERSION}"))
        .build()?;

    let release: GhRelease = client
        .get(format!(
            "https://api.github.com/repos/{RELEASES_REPO}/releases/latest"
        ))
        .send()
        .await
        .context("failed to reach GitHub — are you online?")?
        .json()
        .await
        .context("failed to parse GitHub release response")?;

    let latest = release.tag_name.trim_start_matches('v');

    if !is_newer(latest, CURRENT_VERSION) {
        println!(
            "  {} Already on the latest version ({})\n",
            style("✓").green(),
            style(format!("v{CURRENT_VERSION}")).bold()
        );
        return Ok(());
    }

    println!(
        "  {} Update available: {} → {}\n",
        style("↑").yellow().bold(),
        style(format!("v{CURRENT_VERSION}")).dim(),
        style(format!("v{latest}")).bold().green()
    );

    let artifact = current_artifact()?;
    let url = format!("https://github.com/{RELEASES_REPO}/releases/download/v{latest}/{artifact}");

    println!("  {} Downloading {}...", style("→").cyan(), artifact);

    let response = client
        .get(&url)
        .send()
        .await
        .context("failed to download update")?;

    if !response.status().is_success() {
        anyhow::bail!("download failed: HTTP {}", response.status());
    }

    let bytes = response.bytes().await.context("failed to read download")?;

    let current_exe = std::env::current_exe().context("cannot locate current binary")?;
    let tmp_exe = current_exe.with_extension("new");

    std::fs::write(&tmp_exe, &bytes).context("failed to write new binary")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_exe, std::fs::Permissions::from_mode(0o755))
            .context("failed to set permissions")?;
        std::fs::rename(&tmp_exe, &current_exe).context("failed to replace binary")?;
    }

    #[cfg(windows)]
    {
        let old_exe = current_exe.with_extension("old");
        // Windows won't let you write to a running .exe, but rename is allowed.
        std::fs::rename(&current_exe, &old_exe).context("failed to rename current binary")?;
        std::fs::rename(&tmp_exe, &current_exe).context("failed to install new binary")?;
    }

    println!(
        "  {} Updated to {} — restart root to use the new version\n",
        style("✓").green().bold(),
        style(format!("v{latest}")).bold()
    );

    Ok(())
}

fn current_artifact() -> anyhow::Result<String> {
    let name = match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => "root-linux-amd64",
        ("linux", "aarch64") => "root-linux-arm64",
        ("macos", "x86_64") => "root-macos-amd64",
        ("macos", "aarch64") => "root-macos-arm64",
        ("windows", "x86_64") => "root-windows-amd64.exe",
        (os, arch) => anyhow::bail!("unsupported platform: {os}/{arch}"),
    };
    Ok(name.to_string())
}

fn is_newer(candidate: &str, current: &str) -> bool {
    parse_semver(candidate) > parse_semver(current)
}

fn parse_semver(v: &str) -> (u32, u32, u32) {
    let mut parts = v.split('.').filter_map(|p| p.parse::<u32>().ok());
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}
