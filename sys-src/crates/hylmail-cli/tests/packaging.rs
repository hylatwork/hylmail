//! Guards the packaging templates.
//!
//! `cargo test --workspace` cannot compile an installer - Inno Setup and WiX
//! are Windows tools, `hdiutil` is a macOS one, and `cargo-deb` is a separate
//! binary - so a typo in one of those templates would only surface on release
//! day. These tests instead assert the invariants that hold on any platform:
//! the version, the two launcher names, and the asset files the templates
//! point at. The tests read the repository relative to this crate's manifest,
//! which is why they only run from a checkout.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::path::PathBuf;

/// The repository root: the directory the packaging scripts are run from.
fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(relative: &str) -> String {
    let path = repo().join(relative);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

fn exists(relative: &str) -> bool {
    repo().join(relative).is_file()
}

fn assert_mentions(text: &str, needle: &str, what: &str) {
    assert!(text.contains(needle), "{what} does not mention `{needle}`");
}

/// `{XXXXXXXX-XXXX-XXXX-XXXX-XXXXXXXXXXXX}`, upper case, as both installers
/// spell it.
fn is_guid(value: &str) -> bool {
    let trimmed = value.trim();
    let inner = match trimmed
        .strip_prefix('{')
        .and_then(|rest| rest.strip_suffix('}'))
    {
        Some(inner) => inner,
        None => return false,
    };
    let groups: Vec<&str> = inner.split('-').collect();
    let lengths = [8, 4, 4, 4, 12];
    groups.len() == lengths.len()
        && groups.iter().zip(lengths).all(|(group, length)| {
            group.len() == length
                && group
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
        })
}

/// The Inno `[Setup]` `AppId`, which is written as `AppId={{GUID}`.
fn inno_app_id() -> String {
    let script = read("packaging/windows/hylmail.iss");
    let line = script
        .lines()
        .find(|line| line.starts_with("AppId="))
        .expect("the Inno script has an AppId");
    line.strip_prefix("AppId={")
        .expect("the AppId is written with the doubled brace Inno wants")
        .to_owned()
}

/// The WiX `UpgradeCode` of the MSI.
fn wix_upgrade_code() -> String {
    let source = read("packaging/windows/hylmail.wxs");
    let line = source
        .lines()
        .find(|line| line.contains("HylMailUpgradeCode ="))
        .expect("the WiX source defines an UpgradeCode");
    line.split('"').nth(1).expect("a quoted value").to_owned()
}

/// The lines of a template that actually configure something: `;` comments are
/// dropped from the Inno script, `<!-- ... -->` blocks from the WiX source and
/// the property list. Documentation in a template may name a version; the
/// directives must not.
fn configuration_lines(text: &str) -> String {
    let mut out = String::new();
    let mut in_comment = false;
    for line in text.lines() {
        if in_comment {
            if let Some((_, rest)) = line.split_once("-->") {
                in_comment = false;
                out.push_str(rest);
                out.push('\n');
            }
            continue;
        }
        if line.trim_start().starts_with(';') {
            continue;
        }
        if let Some((before, after)) = line.split_once("<!--") {
            out.push_str(before);
            match after.split_once("-->") {
                Some((_, rest)) => {
                    out.push_str(rest);
                    out.push('\n');
                }
                None => {
                    in_comment = true;
                    out.push('\n');
                }
            }
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[test]
fn every_installer_names_both_launchers() {
    let inno = read("packaging/windows/hylmail.iss");
    assert_mentions(&inno, r#""hylmail-gui.exe""#, "the Inno script");
    assert_mentions(&inno, r#""hylmail-cli.exe""#, "the Inno script");

    let wix = read("packaging/windows/hylmail.wxs");
    assert_mentions(
        &wix,
        r#"HylMailGuiExe = "hylmail-gui.exe""#,
        "the WiX source",
    );
    assert_mentions(
        &wix,
        r#"HylMailCliExe = "hylmail-cli.exe""#,
        "the WiX source",
    );

    let plist = read("packaging/macos/Info.plist");
    let executable = plist
        .lines()
        .skip_while(|line| !line.contains("CFBundleExecutable"))
        .nth(1)
        .expect("CFBundleExecutable has a value");
    assert_eq!(executable.trim(), "<string>hylmail-gui</string>");

    let app = read("packaging/macos/make-app.sh");
    assert_mentions(&app, "hylmail-gui", "make-app.sh");
    assert_mentions(&app, "hylmail-cli", "make-app.sh");

    let desktop = read("packaging/linux/de.hylmail.HylMail.desktop");
    assert_mentions(&desktop, "Exec=hylmail-gui ", "the desktop entry");
}

#[test]
fn the_version_comes_from_the_workspace() {
    let version = env!("CARGO_PKG_VERSION");

    let inno = read("packaging/windows/hylmail.iss");
    assert_mentions(&inno, "#ifndef HylMailVersion", "the Inno script");
    assert_mentions(&inno, "AppVersion={#HylMailVersion}", "the Inno script");
    assert_mentions(
        &inno,
        "OutputBaseFilename=HylMail-{#HylMailVersion}-setup",
        "the Inno script",
    );

    let wix = read("packaging/windows/hylmail.wxs");
    assert_mentions(&wix, "<?ifndef HylMailVersion ?>", "the WiX source");
    assert_mentions(&wix, r#"Version="$(var.HylMailVersion)""#, "the WiX source");

    let plist = read("packaging/macos/Info.plist");
    assert_eq!(
        plist.matches("<string>{{VERSION}}</string>").count(),
        2,
        "CFBundleShortVersionString and CFBundleVersion both take the version"
    );
    for key in ["CFBundleShortVersionString", "CFBundleVersion"] {
        assert_mentions(&plist, key, "the property list");
    }

    // The placeholder in the two preprocessor guards is allowed to be 0.0.0;
    // anything else would be a hard-coded version that no release would update.
    if version != "0.0.0" {
        for (name, text) in [
            ("packaging/windows/hylmail.iss", inno),
            ("packaging/windows/hylmail.wxs", wix),
            ("packaging/macos/Info.plist", plist),
        ] {
            let directives = configuration_lines(&text);
            assert!(
                !directives.contains(version),
                "{name} hard-codes the version {version}"
            );
        }
    }

    let workflow = read(".github/workflows/release.yml");
    assert_mentions(&workflow, "cargo metadata", "the release workflow");
    assert_mentions(&workflow, "HylMail-$VERSION", "the release workflow");
}

#[test]
fn the_installers_use_two_different_identities() {
    let app_id = inno_app_id();
    assert!(is_guid(&app_id), "the Inno AppId `{app_id}` is not a GUID");

    let upgrade_code = wix_upgrade_code();
    assert!(
        is_guid(&upgrade_code),
        "the WiX UpgradeCode `{upgrade_code}` is not a GUID"
    );
    assert_ne!(
        app_id, upgrade_code,
        "the MSI and the Inno installer have to be separate products"
    );

    let plist = read("packaging/macos/Info.plist");
    assert!(
        plist.contains(&format!("<string>{}</string>", hylmail_gui::BUNDLE_ID)),
        "the bundle identifier is not hylmail_gui::BUNDLE_ID"
    );

    // The MSI installs with the standard progress dialog: no `WixUIExtension`,
    // which would need an RTF licence page.
    let wix = read("packaging/windows/hylmail.wxs");
    assert_mentions(&wix, "<MediaTemplate", "the WiX source");
    for ui in ["UIRef", "WixUIExtension"] {
        assert!(
            !configuration_lines(&wix).contains(ui),
            "the WiX source references {ui}"
        );
    }
}

#[test]
fn every_icon_and_licence_a_template_names_exists() {
    for asset in [
        "packaging/icons/hylmail.ico",
        "packaging/icons/HylMail.icns",
        "packaging/icons/hylmail.svg",
        "LICENSE",
    ] {
        assert!(exists(asset), "{asset} is missing");
    }

    let inno = read("packaging/windows/hylmail.iss");
    assert_mentions(&inno, r"..\icons\hylmail.ico", "the Inno script");
    assert_mentions(&inno, r"..\..\LICENSE", "the Inno script");

    let wix = read("packaging/windows/hylmail.wxs");
    assert_mentions(&wix, r"packaging\icons\hylmail.ico", "the WiX source");
    assert_mentions(&wix, r#"Source="LICENSE""#, "the WiX source");

    let app = read("packaging/macos/make-app.sh");
    assert_mentions(&app, "HylMail.icns", "make-app.sh");
    assert_mentions(&app, r#""$root/LICENSE""#, "make-app.sh");

    // The desktop entry has to name the icon the deb and the tarball install.
    let desktop = read("packaging/linux/de.hylmail.HylMail.desktop");
    assert_mentions(&desktop, "Icon=de.hylmail.HylMail\n", "the desktop entry");
}

/// The asset list in `[package.metadata.deb]`, as `(source, destination, mode)`.
/// The sources are relative to this crate's manifest; `target/` means the
/// workspace target directory.
fn deb_assets() -> Vec<(String, String, String)> {
    let manifest = read("crates/hylmail-cli/Cargo.toml");
    let mut assets = Vec::new();
    let mut inside = false;
    for line in manifest.lines() {
        if line.starts_with("assets = [") {
            inside = true;
            continue;
        }
        if inside && line.starts_with(']') {
            break;
        }
        if !inside {
            continue;
        }
        let quoted: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
        if quoted.len() >= 3 {
            assets.push((
                quoted[0].to_owned(),
                quoted[1].to_owned(),
                quoted[2].to_owned(),
            ));
        }
    }
    assets
}

/// The asset that installs `source`, if any.
fn asset<'a>(
    assets: &'a [(String, String, String)],
    source: &str,
) -> Option<&'a (String, String, String)> {
    assets.iter().find(|(entry, _, _)| entry == source)
}

#[test]
fn the_deb_assets_cover_the_binaries_the_icons_and_the_desktop_entry() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let assets = deb_assets();
    assert!(!assets.is_empty(), "the deb metadata lists no assets");

    for (source, _, _) in &assets {
        // The two launchers are built into the workspace target directory.
        if source.starts_with("target/") {
            continue;
        }
        let path = manifest_dir.join(source);
        assert!(
            path.is_file(),
            "`{source}` is installed by the deb but does not exist"
        );
    }

    for binary in ["target/release/hylmail-gui", "target/release/hylmail-cli"] {
        match asset(&assets, binary) {
            Some((_, destination, mode)) => {
                assert_eq!(destination, "usr/bin/", "{binary} is installed elsewhere");
                assert_eq!(mode, "755", "{binary} is not executable");
            }
            None => panic!("the deb does not install {binary}"),
        }
    }

    match asset(&assets, "../../packaging/linux/de.hylmail.HylMail.desktop") {
        Some((_, destination, _)) => assert_eq!(destination, "usr/share/applications/"),
        None => panic!("the deb does not install the desktop entry"),
    }

    // Every raster the icon generator writes is shipped to the Linux desktops;
    // 1024 is not a size freedesktop.org defines.
    for size in [16, 24, 32, 48, 64, 128, 256, 512] {
        let source = format!("../../packaging/icons/hylmail-{size}.png");
        let destination =
            format!("usr/share/icons/hicolor/{size}x{size}/apps/de.hylmail.HylMail.png");
        match asset(&assets, &source) {
            Some((_, installed, _)) => assert_eq!(installed, &destination),
            None => panic!("the deb does not install the {size} pixel icon"),
        }
    }

    // The binaries open these through `dlopen`, so `$auto` cannot see them.
    let manifest = read("crates/hylmail-cli/Cargo.toml");
    assert_mentions(&manifest, "depends = \"$auto,", "the deb metadata");
    for library in ["libx11-6", "libxkbcommon0", "libwayland-client0", "libgl1"] {
        assert_mentions(&manifest, library, "the deb metadata");
    }
}

#[test]
fn the_packaging_scripts_are_runnable() {
    // The first line decides whether `./script.sh` works at all; a stray byte
    // or a Windows line ending turns the script into a text file that the
    // shell refuses to run.
    for script in [
        "packaging/macos/make-app.sh",
        "packaging/macos/make-dmg.sh",
        "packaging/linux/make-tarball.sh",
    ] {
        let text = read(script);
        assert_eq!(
            text.lines().next().unwrap_or_default(),
            "#!/usr/bin/env bash",
            "{script} has no bash shebang"
        );
        assert!(!text.contains("\r\n"), "{script} has Windows line endings");
    }

    // The release workflow calls them as `packaging/...`, so they have to keep
    // their executable bit.  Windows has no such bit, which is why the test
    // only looks at it where it exists.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        for script in [
            "packaging/macos/make-app.sh",
            "packaging/macos/make-dmg.sh",
            "packaging/linux/make-tarball.sh",
        ] {
            let metadata = fs::metadata(repo().join(script))
                .unwrap_or_else(|error| panic!("cannot stat {script}: {error}"));
            assert!(
                metadata.permissions().mode() & 0o111 != 0,
                "{script} is not executable"
            );
        }
    }
}

#[test]
fn the_release_workflow_builds_every_package() {
    let workflow = read(".github/workflows/release.yml");
    // The Windows steps are PowerShell, so they spell their paths with
    // backslashes; the macOS and Linux steps run in bash.
    for input in [
        "hylmail.iss",
        "hylmail.wxs",
        "make-app.sh",
        "make-dmg.sh",
        "make-tarball.sh",
    ] {
        assert_mentions(&workflow, input, "the release workflow");
    }
    assert_mentions(
        &workflow,
        "cargo deb -p hylmail-cli",
        "the release workflow",
    );
    for tool in ["ISCC.exe", "candle.exe", "light.exe"] {
        assert_mentions(&workflow, tool, "the release workflow");
    }
    assert_mentions(&workflow, "action-gh-release", "the release workflow");
    assert_mentions(
        &workflow,
        "if-no-files-found: error",
        "the release workflow",
    );
}
