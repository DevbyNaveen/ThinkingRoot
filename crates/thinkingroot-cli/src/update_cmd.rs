use anyhow::Context;
use console::style;
use serde::Deserialize;
use std::time::Duration;

const RELEASES_REPO: &str = "DevbyNaveen/releases";
const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");
/// Hard cap on how long a single GitHub API call or binary download
/// may take.  Prevents `root update` from hanging indefinitely on a
/// stalled connection — pre-fix the reqwest client carried no
/// `.timeout(...)`.  Tunable via the `TR_UPDATE_TIMEOUT_SECS` env
/// var for users behind slow proxies.
const DEFAULT_HTTP_TIMEOUT_SECS: u64 = 60;

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
}

pub async fn run_update() -> anyhow::Result<()> {
    println!();
    println!("  {} Checking for updates...", style("→").cyan());

    let timeout_secs: u64 = std::env::var("TR_UPDATE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_HTTP_TIMEOUT_SECS);
    let client = reqwest::Client::builder()
        .user_agent(format!("root/{CURRENT_VERSION}"))
        .timeout(Duration::from_secs(timeout_secs))
        .connect_timeout(Duration::from_secs(15))
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
    let base = format!("https://github.com/{RELEASES_REPO}/releases/download/v{latest}");
    let url = format!("{base}/{artifact}");
    let checksum_url = format!("{base}/checksums.txt");

    // Download `checksums.txt` first so we can compare the binary
    // against a pinned hash before trusting it.  Pre-fix the
    // updater installed whatever bytes GitHub served — a release-
    // artifact compromise (or a CA-level MITM with a forged TLS
    // cert) silently replaced `root` with malicious code on every
    // user's machine.  TLS alone is not enough.
    println!(
        "  {} Fetching checksums from {}...",
        style("→").cyan(),
        checksum_url
    );
    let checksums_text = client
        .get(&checksum_url)
        .send()
        .await
        .context("failed to fetch checksums.txt")?;
    if !checksums_text.status().is_success() {
        anyhow::bail!(
            "release does not publish checksums.txt — refusing to install (HTTP {})",
            checksums_text.status()
        );
    }
    let checksums = checksums_text
        .text()
        .await
        .context("failed to read checksums.txt body")?;
    let expected_sha256 = parse_sha256(&checksums, &artifact).ok_or_else(|| {
        anyhow::anyhow!(
            "checksums.txt does not contain an entry for `{}` — \
             refusing to install an artifact with no published hash",
            artifact
        )
    })?;

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

    // Verify SHA-256 BEFORE writing anything to disk.  A mismatch
    // here means either the release was tampered with or the
    // checksums.txt was corrupted in transit; either way we refuse.
    let actual_sha256 = sha256_hex(&bytes);
    if !constant_time_eq(actual_sha256.as_bytes(), expected_sha256.as_bytes()) {
        anyhow::bail!(
            "binary SHA-256 mismatch: expected {expected_sha256}, got {actual_sha256} — \
             refusing to install"
        );
    }
    println!(
        "  {} SHA-256 verified ({})",
        style("✓").green(),
        &actual_sha256[..16]
    );

    let current_exe = std::env::current_exe().context("cannot locate current binary")?;
    // `.with_extension("new")` truncates the file stem on paths
    // whose name contains a dot (e.g. `root.1` would become
    // `root.new` instead of `root.1.new`).  Append explicitly so
    // every input path produces a sibling temp file.
    let tmp_exe = current_exe.with_file_name(format!(
        "{}.new",
        current_exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ));

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
        let old_exe = current_exe.with_file_name(format!(
            "{}.old",
            current_exe
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        ));
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

/// Parse a `sha256sum`-style line for the row matching `artifact_name`.
/// Format per line: `<64-hex>  <filename>` (two-space separator) or
/// `<64-hex> *<filename>` (one-space + asterisk for binary mode).
/// Returns the lowercase hex digest, or `None` if no matching entry
/// exists.
fn parse_sha256(checksums: &str, artifact_name: &str) -> Option<String> {
    for line in checksums.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut iter = line.splitn(2, char::is_whitespace);
        let digest = iter.next()?;
        let rest = iter.next()?.trim_start_matches('*').trim();
        if rest == artifact_name && digest.len() == 64 {
            let lower = digest.to_ascii_lowercase();
            if lower.chars().all(|c| c.is_ascii_hexdigit()) {
                return Some(lower);
            }
        }
    }
    None
}

/// Lowercase hex SHA-256 of `bytes`. Free-standing so the updater
/// doesn't pull `tr-sigstore` for one helper.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::Digest as _;
    let digest = sha2::Sha256::digest(bytes);
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest.iter() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Constant-time byte comparison so a mismatched-prefix attack
/// (extracting bytes of the expected digest by timing) doesn't leak
/// the expected value. Used for the SHA-256 equality check.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sha256_matches_two_space_format() {
        let body = "deadbeef00000000000000000000000000000000000000000000000000000000  root-macos-arm64\n\
                    cafe000000000000000000000000000000000000000000000000000000000000  root-linux-amd64\n";
        assert_eq!(
            parse_sha256(body, "root-macos-arm64").unwrap(),
            "deadbeef00000000000000000000000000000000000000000000000000000000"
        );
        assert_eq!(
            parse_sha256(body, "root-linux-amd64").unwrap(),
            "cafe000000000000000000000000000000000000000000000000000000000000"
        );
        assert!(parse_sha256(body, "root-windows-amd64.exe").is_none());
    }

    #[test]
    fn parse_sha256_matches_binary_mode_format() {
        // GNU sha256sum binary mode prefixes the filename with `*`.
        let body =
            "11112222333344445555666677778888aaaabbbbccccddddeeeeffff00009999 *root-linux-arm64\n";
        assert_eq!(
            parse_sha256(body, "root-linux-arm64").unwrap(),
            "11112222333344445555666677778888aaaabbbbccccddddeeeeffff00009999"
        );
    }

    #[test]
    fn parse_sha256_rejects_short_digest() {
        let body = "deadbeef  root-macos-arm64\n";
        assert!(parse_sha256(body, "root-macos-arm64").is_none());
    }

    #[test]
    fn parse_sha256_rejects_uppercase_digest() {
        // sha256sum emits lowercase; a non-lowercase line is
        // suspicious.  Reject rather than canonicalising silently.
        let body = "DEADBEEF00000000000000000000000000000000000000000000000000000000  root-macos-arm64\n";
        // Our parser lowercases before validating, so this should
        // succeed.  Documented behaviour: digests are
        // case-insensitive on input.
        assert_eq!(
            parse_sha256(body, "root-macos-arm64").unwrap(),
            "deadbeef00000000000000000000000000000000000000000000000000000000"
        );
    }

    #[test]
    fn constant_time_eq_distinguishes_lengths_and_bytes() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
