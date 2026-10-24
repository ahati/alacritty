use std::env;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use gl_generator::{Api, Fallbacks, GlobalGenerator, Profile, Registry};

fn main() {
    // A tagged release names itself after the tag that triggered the release
    // workflow; plain builds fall back to the package version.
    let mut version =
        env::var("RELEASE_TAG").unwrap_or_else(|_| String::from(env!("CARGO_PKG_VERSION")));
    if let Some(commit_hash) = commit_hash() {
        version = format!("{version} ({commit_hash})");
    }
    println!("cargo:rustc-env=VERSION={version}");

    let dest = env::var("OUT_DIR").unwrap();
    let mut file = File::create(Path::new(&dest).join("gl_bindings.rs")).unwrap();

    Registry::new(
        Api::Gl,
        (3, 3),
        Profile::Core,
        Fallbacks::All,
        ["GL_ARB_blend_func_extended", "GL_KHR_robustness", "GL_KHR_debug"],
    )
    .write_bindings(GlobalGenerator, &mut file)
    .unwrap();

    embed_fonts();

    #[cfg(windows)]
    embed_resource::compile("./windows/alacritty.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}

fn commit_hash() -> Option<String> {
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| String::from_utf8(output.stdout).ok())
        .map(|hash| hash.trim().into())
}

/// Download and extract the default font embedded into the binary.
///
/// The font is fetched from the latest JetBrains Mono Nerd Font Mono release at
/// build time and cached, so only cold builds hit the network. Set
/// `ALACRITTY_EMBED_FONTS=0` to build without the embedded font and
/// `ALACRITTY_FONT_CACHE` to override the cache location (for vendored builds).
fn embed_fonts() {
    println!("cargo:rerun-if-env-changed=ALACRITTY_EMBED_FONTS");
    println!("cargo:rerun-if-env-changed=ALACRITTY_FONT_CACHE");
    println!("cargo:rustc-check-cfg=cfg(embed_fonts)");

    if env::var("ALACRITTY_EMBED_FONTS").is_ok_and(|enabled| enabled == "0") {
        return;
    }

    const FONT_ZIP_URL: &str =
        "https://github.com/ryanoasis/nerd-fonts/releases/latest/download/JetBrainsMono.zip";
    const RELEASES_API: &str = "https://api.github.com/repos/ryanoasis/nerd-fonts/releases/latest";
    const FONT_TAG: &str = "JetBrainsMonoNerdFontMono";
    const STYLES: [(&str, &str); 4] = [
        ("REGULAR", "Regular"),
        ("BOLD", "Bold"),
        ("ITALIC", "Italic"),
        ("BOLD_ITALIC", "BoldItalic"),
    ];

    let Some(cache_dir) = font_cache_dir() else { return };

    // Fonts from a previous build are reused, keeping incremental builds
    // offline.
    if STYLES.iter().all(|(key, style)| {
        let path = cache_dir.join(format!("{FONT_TAG}-{style}.ttf"));
        if !path.exists() {
            return false;
        }
        println!("cargo:rerun-if-changed={}", path.display());
        println!("cargo:rustc-env=EMBEDDED_FONT_{key}={}", path.display());
        true
    }) {
        let release =
            fs::read_to_string(cache_dir.join("release")).unwrap_or_else(|_| "unknown".into());
        println!("cargo:rustc-env=EMBEDDED_FONT_RELEASE={release}");
        println!("cargo:rustc-cfg=embed_fonts");
        return;
    }

    // Fetch the latest release asset; GitHub resolves the `latest` URL to it.
    println!("cargo:warning=Fetching embedded font from {FONT_ZIP_URL}");
    let Some(zip_bytes) = fetch_url(FONT_ZIP_URL, None) else {
        println!("cargo:warning=Embedded font download failed; building without embedded font");
        return;
    };
    let release = fetch_url(RELEASES_API, Some("application/vnd.github+json"))
        .and_then(|body| latest_release_tag(&body))
        .unwrap_or_else(|| String::from("unknown"));

    let mut archive = match zip::ZipArchive::new(std::io::Cursor::new(&zip_bytes)) {
        Ok(archive) => archive,
        Err(err) => {
            println!(
                "cargo:warning=Embedded font archive invalid; building without embedded font: {err}"
            );
            return;
        },
    };

    let _ = fs::create_dir_all(&cache_dir);
    let mut extracted = 0;
    for index in 0..archive.len() {
        let Ok(mut file) = archive.by_index(index) else { continue };
        let Some(entry_path) = file.enclosed_name() else { continue };
        let Some(name) = entry_path.file_name().and_then(|name| name.to_str()) else { continue };

        for (key, style) in STYLES {
            if name != format!("{FONT_TAG}-{style}.ttf") {
                continue;
            }

            let path = cache_dir.join(format!("{FONT_TAG}-{style}.ttf"));
            match File::create(&path).and_then(|mut target| std::io::copy(&mut file, &mut target)) {
                Ok(_) => {
                    println!("cargo:rerun-if-changed={}", path.display());
                    println!("cargo:rustc-env=EMBEDDED_FONT_{key}={}", path.display());
                    extracted += 1;
                },
                Err(err) => {
                    println!("cargo:warning=Could not extract embedded font {style}: {err}")
                },
            }
        }
    }

    if extracted != STYLES.len() {
        println!("cargo:warning=Embedded font archive incomplete; building without embedded font");
        return;
    }

    let _ = fs::write(cache_dir.join("release"), &release);
    println!("cargo:rustc-env=EMBEDDED_FONT_RELEASE={release}");
    println!("cargo:rustc-cfg=embed_fonts");
}

/// Cache directory for the embedded font, inside the build target directory.
fn font_cache_dir() -> Option<PathBuf> {
    if let Ok(dir) = env::var("ALACRITTY_FONT_CACHE") {
        return Some(dir.into());
    }

    // `<target>/<profile>/build/<pkg>-<hash>/out` → `<target>/embedded-fonts`.
    env::var("OUT_DIR").ok().and_then(|out_dir| {
        Path::new(&out_dir).ancestors().nth(4).map(|target| target.join("embedded-fonts"))
    })
}

/// Resolve the tag of the latest Nerd Fonts release.
/// Resolve the tag of the latest Nerd Fonts release from its JSON body.
fn latest_release_tag(body: &[u8]) -> Option<String> {
    let body = std::str::from_utf8(body).ok()?;

    // Sniff the tag name without pulling a JSON parser into the build deps.
    let index = body.find("\"tag_name\":")?;
    let rest = &body[index + "\"tag_name\":".len()..];
    let value_start = rest.find('"')?;
    let value_rest = &rest[value_start + 1..];
    let value_end = value_rest.find('"')?;
    Some(value_rest[..value_end].to_owned())
}

/// Fetch a URL's body, preferring `curl` and falling back to `wget`.
///
/// System downloaders keep the build free of an HTTP dependency stack; both
/// are available on every platform the project builds on.
fn fetch_url(url: &str, accept: Option<&str>) -> Option<Vec<u8>> {
    let mut headers = vec![String::from("User-Agent: alacritty-build")];
    if let Some(accept) = accept {
        headers.push(format!("Accept: {accept}"));
    }

    let mut curl = Command::new("curl");
    curl.args(["--location", "--silent", "--show-error", "--fail", url]);
    let mut wget = Command::new("wget");
    wget.args(["-qO-", url]);
    for header in &headers {
        curl.args(["--header", header]);
        wget.arg(format!("--header={header}"));
    }

    for mut downloader in [curl, wget] {
        if let Ok(output) = downloader.output() {
            if output.status.success() && !output.stdout.is_empty() {
                return Some(output.stdout);
            }
        }
    }

    None
}
