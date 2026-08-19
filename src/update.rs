use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use sha2::{Digest, Sha256};

#[cfg(unix)]
const MANIFEST_LIMIT: usize = 64 * 1024;
#[cfg(unix)]
const SIGNATURE_LIMIT: usize = 512;
#[cfg(unix)]
const BINARY_LIMIT: usize = 128 * 1024 * 1024;
#[cfg(unix)]
const STABLE_UPDATE_BASE: &str = "https://github.com/vivido-dev/vvmux/releases/latest/download";
#[cfg(unix)]
const PREVIEW_UPDATE_BASE: &str = "https://github.com/vivido-dev/vvmux/releases/download/preview";
// Local/development builds intentionally use a fail-closed key. Official builds inject the stable
// public key with VVMUX_UPDATE_PUBLIC_KEY_HEX; the signing seed remains only in release secrets.
#[cfg(unix)]
const DEVELOPMENT_PUBLIC_KEY: &str =
    "02775e51141d0f0f8fa5aac49eab5f36e3ef2c8eb71f7d7984516b167c9a8de8";

#[derive(Debug, Clone, Copy, Serialize, Deserialize, ValueEnum, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Channel {
    Stable,
    Preview,
}

impl Channel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Preview => "preview",
        }
    }
}

#[derive(Debug, Subcommand)]
pub(crate) enum ChannelCommand {
    /// Persist the update channel for this user.
    Set { channel: Channel },
    /// Print the current update channel.
    Show,
}

#[cfg(unix)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateManifest {
    schema: u16,
    channel: Channel,
    version: semver::Version,
    notes_url: String,
    assets: Vec<UpdateAsset>,
}

#[cfg(unix)]
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdateAsset {
    target: String,
    url: String,
    sha256: String,
    size: u64,
}

pub(crate) fn channel(command: ChannelCommand) -> io::Result<()> {
    match command {
        ChannelCommand::Set { channel } => {
            write_channel(channel)?;
            println!("vvmux update channel set to {}", channel.as_str());
        }
        ChannelCommand::Show => println!("{}", read_channel()?.as_str()),
    }
    Ok(())
}

#[cfg(windows)]
pub(crate) fn run(_check: bool) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Windows vvmux updates are managed by the Vivido Suite installer",
    ))
}

#[cfg(unix)]
pub(crate) fn run(check: bool) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let channel = read_channel()?;
    let base = option_env!("VVMUX_UPDATE_BASE_URL").unwrap_or(match channel {
        Channel::Stable => STABLE_UPDATE_BASE,
        Channel::Preview => PREVIEW_UPDATE_BASE,
    });
    let manifest_url = format!("{base}/vvmux-update-{}.json", channel.as_str());
    let manifest_bytes = download(&manifest_url, MANIFEST_LIMIT)?;
    let signature_text =
        String::from_utf8(download(&format!("{manifest_url}.sig"), SIGNATURE_LIMIT)?)
            .map_err(|_| invalid("update signature is not UTF-8"))?;
    let public_hex = option_env!("VVMUX_UPDATE_PUBLIC_KEY_HEX").unwrap_or(DEVELOPMENT_PUBLIC_KEY);
    verify_manifest_signature(&manifest_bytes, &signature_text, public_hex)?;

    let manifest: UpdateManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| invalid(format!("invalid update manifest: {error}")))?;
    validate_manifest(&manifest, channel)?;
    let current = semver::Version::parse(env!("CARGO_PKG_VERSION")).map_err(io::Error::other)?;
    if manifest.version <= current {
        println!("vvmux {current} is current on {}", channel.as_str());
        return Ok(());
    }
    let asset = manifest
        .assets
        .iter()
        .find(|asset| asset.target == target())
        .ok_or_else(|| invalid(format!("release has no asset for {}", target())))?;
    println!(
        "vvmux {} is available on {} ({})",
        manifest.version,
        channel.as_str(),
        manifest.notes_url
    );
    if check {
        return Ok(());
    }

    let executable = std::env::current_exe()?;
    let metadata = fs::metadata(&executable)?;
    if !metadata.is_file() || metadata.uid() != unsafe { libc::geteuid() } {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to replace an executable not owned by the current user",
        ));
    }
    let bytes = download(&asset.url, BINARY_LIMIT)?;
    if bytes.len() as u64 != asset.size {
        return Err(invalid(
            "downloaded update size differs from the signed manifest",
        ));
    }
    let digest = hex::encode(Sha256::digest(&bytes));
    if !digest.eq_ignore_ascii_case(&asset.sha256) {
        return Err(invalid(
            "downloaded update digest differs from the signed manifest",
        ));
    }
    let parent = executable
        .parent()
        .ok_or_else(|| invalid("current executable has no parent directory"))?;
    let temporary = parent.join(format!(".vvmux-update-{}", crate::plugin::random_id()?));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true).mode(0o700);
    let mut file = options.open(&temporary)?;
    if let Err(error) = (|| -> io::Result<()> {
        file.write_all(&bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, &executable)?;
        sync_directory(parent)?;
        Ok(())
    })() {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    println!(
        "updated vvmux to {}; running sessions continue on their existing daemon until restarted",
        manifest.version
    );
    Ok(())
}

#[cfg(unix)]
fn verify_manifest_signature(
    bytes: &[u8],
    signature_text: &str,
    public_hex: &str,
) -> io::Result<()> {
    use base64::Engine;
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};

    let signature_bytes = base64::engine::general_purpose::STANDARD
        .decode(signature_text.trim())
        .map_err(|_| invalid("update signature is not valid base64"))?;
    let signature = Signature::from_slice(&signature_bytes)
        .map_err(|_| invalid("update signature has the wrong length"))?;
    let public: [u8; 32] = hex::decode(public_hex)
        .map_err(|_| invalid("compiled update public key is invalid hex"))?
        .try_into()
        .map_err(|_| invalid("compiled update public key has the wrong length"))?;
    VerifyingKey::from_bytes(&public)
        .map_err(|_| invalid("compiled update public key is invalid"))?
        .verify(bytes, &signature)
        .map_err(|_| invalid("update manifest signature verification failed"))?;
    Ok(())
}

#[cfg(unix)]
fn validate_manifest(manifest: &UpdateManifest, expected: Channel) -> io::Result<()> {
    let mut targets = std::collections::BTreeSet::new();
    if manifest.schema != 1 || manifest.channel != expected {
        return Err(invalid("update manifest schema or channel mismatch"));
    }
    if !manifest.notes_url.starts_with("https://") || manifest.assets.len() > 16 {
        return Err(invalid("update manifest contains invalid release metadata"));
    }
    for asset in &manifest.assets {
        if !targets.insert(asset.target.as_str())
            || asset.target.is_empty()
            || asset.target.len() > 128
            || !asset.url.starts_with("https://")
            || asset.sha256.len() != 64
            || !asset.sha256.bytes().all(|byte| byte.is_ascii_hexdigit())
            || asset.size == 0
            || asset.size > BINARY_LIMIT as u64
        {
            return Err(invalid("update manifest contains an invalid asset"));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn download(url: &str, limit: usize) -> io::Result<Vec<u8>> {
    if !url.starts_with("https://") {
        return Err(invalid("update downloads require HTTPS"));
    }
    let mut response = ureq::get(url)
        .call()
        .map_err(|error| io::Error::other(format!("update download failed: {error}")))?;
    response
        .body_mut()
        .with_config()
        .limit(limit as u64)
        .read_to_vec()
        .map_err(|error| io::Error::other(format!("update download failed: {error}")))
}

fn channel_path() -> io::Result<PathBuf> {
    crate::config::config_dir()
        .map(|directory| directory.join("update-channel"))
        .ok_or_else(|| invalid("could not determine the vvmux configuration directory"))
}

fn read_channel() -> io::Result<Channel> {
    match fs::read_to_string(channel_path()?) {
        Ok(value) if value.trim() == "preview" => Ok(Channel::Preview),
        Ok(value) if value.trim() == "stable" => Ok(Channel::Stable),
        Ok(_) => Err(invalid(
            "update channel file must contain stable or preview",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Channel::Stable),
        Err(error) => Err(error),
    }
}

fn write_channel(channel: Channel) -> io::Result<()> {
    let path = channel_path()?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("update channel path has no parent"))?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    if let Err(error) = (|| -> io::Result<()> {
        writeln!(file, "{}", channel.as_str())?;
        file.sync_all()?;
        drop(file);
        crate::runtime::atomic_replace(&temporary, &path)?;
        sync_directory(parent)
    })() {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }
    Ok(())
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> io::Result<()> {
    fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn target() -> &'static str {
    "x86_64-unknown-linux-gnu"
}
#[cfg(all(target_os = "linux", target_arch = "aarch64"))]
fn target() -> &'static str {
    "aarch64-unknown-linux-gnu"
}
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
fn target() -> &'static str {
    "x86_64-apple-darwin"
}
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
fn target() -> &'static str {
    "aarch64-apple-darwin"
}
#[cfg(all(
    unix,
    not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "x86_64"),
        all(target_os = "macos", target_arch = "aarch64")
    ))
))]
fn target() -> &'static str {
    "unsupported"
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn manifest_signature_is_verified_before_parsing() {
        use base64::Engine;
        use ed25519_dalek::{Signer, SigningKey};

        let signing = SigningKey::from_bytes(&[7; 32]);
        let bytes = br#"{"schema":1}"#;
        let signature =
            base64::engine::general_purpose::STANDARD.encode(signing.sign(bytes).to_bytes());
        let public = hex::encode(signing.verifying_key().to_bytes());
        verify_manifest_signature(bytes, &signature, &public).unwrap();
        assert!(verify_manifest_signature(b"altered", &signature, &public).is_err());
    }

    #[test]
    fn rejects_unsigned_manifest_metadata_before_asset_selection() {
        let manifest = UpdateManifest {
            schema: 1,
            channel: Channel::Stable,
            version: semver::Version::new(1, 0, 0),
            notes_url: "http://insecure.test/notes".into(),
            assets: Vec::new(),
        };
        assert!(validate_manifest(&manifest, Channel::Stable).is_err());
    }
}
