# Installing this fork with winget

`winget install Alacritty.Alacritty` installs **upstream** Alacritty from the
community repository. It will never resolve to this fork, which is not published
there. What works instead is winget's local-manifest mode, which installs from
manifest files kept in this directory.

## What winget requires

- **The installer must be a URL.** winget downloads the artifact from
  `InstallerUrl`; it never accepts a local file, even with local manifests
  enabled. So a release has to exist somewhere reachable — the release assets
  this repository's workflow already builds are the intended source.
- **The SHA256 must match that artifact exactly.** `generate.ps1` hashes the
  artifact it will name, so the two cannot drift apart.

## Producing the artifacts

`.github/workflows/release.yml` runs on a tag matching
`v<major>.<minor>.<patch>` and publishes:

| Asset | Used by |
| --- | --- |
| `Alacritty-vX.Y.Z-portable.exe` | the portable manifest (default) |
| `Alacritty-vX.Y.Z-installer.msi` | the MSI manifest |

So the first step is to push a tag to **this fork** (nothing is sent upstream):

```sh
git tag v0.18.0 && git push origin v0.18.0
```

Wait for the release job to finish, then check the assets exist on the fork's
release page.

## Generating the manifest

On Windows, in PowerShell:

```powershell
# Portable executable: no administrator rights needed to install, and it does
# not touch an existing upstream Alacritty installation.
.\winget\generate.ps1 -Version 0.18.0
```

The script verifies the version is numeric, downloads the release asset to hash
it, and renders `winget/manifests/` from the templates in `winget/templates/`.
Use `-Artifact <path>` to hash a local build instead of downloading.

## Installing

Once per machine, as an administrator:

```powershell
winget settings --enable LocalManifestFiles
```

Then, as the user who wants the terminal:

```powershell
winget install --manifest .\winget\manifests
winget uninstall --id ahati.Alacritty
```

## Caveats worth knowing

- **Nothing is published.** These manifests are read from disk. `winget upgrade`
  will not track this package, and winget will not treat it as an update of the
  upstream package: both can be installed side by side.
- **The two packages cannot be confused.** The identifier `ahati.Alacritty`
  differs from upstream's `Alacritty.Alacritty`, and the moniker `alacritty-fork`
  from `alacritty`, so `winget list` shows the two separately and
  `winget install alacritty` stays unambiguous.
- **The binaries are unsigned**, so SmartScreen warns on first run.
- **Local manifests are trusted wholesale.** Enabling `LocalManifestFiles`
  allows *any* manifest on the machine to be installed this way, which is why
  winget gates it behind an administrator. Turn it back off with
  `winget settings --disable LocalManifestFiles` when you are done.
- **The generated manifests are not committed.** They embed the SHA256 of one
  specific artifact, so they describe a release rather than the source tree;
  `winget/manifests/` is ignored by git and rebuilt by the script.
- **The MSI path needs two fixes first.** `alacritty/windows/wix/alacritty.wxs`
  still carries upstream's `UpgradeCode`
  (`87c21c74-dbd5-4584-89d5-46d9cd0c40a7`) and `Manufacturer="Alacritty"`, so
  Windows Installer treats this fork as *the same product* as upstream Alacritty
  and would replace it rather than install alongside. It also carries
  `Version="0.18.0-dev"`, which is not a valid MSI version — MSI wants digits and
  dots only. Until those are changed, prefer the portable manifest, which has no
  such identity.
- **winget's portable install uses links** under
  `%LOCALAPPDATA%\Microsoft\WinGet\Links`; on some systems that needs Developer
  Mode. This has not been verified here — check `winget install` output.
