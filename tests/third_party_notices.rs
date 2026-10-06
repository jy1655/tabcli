use std::{collections::BTreeSet, fs, path::Path, process::Command};

use anyhow::{Context, Result, bail, ensure};

type Packages = BTreeSet<(String, String)>;

const HEADER: &str = "# Third-party notices\n\n\
This file records the licences of every third-party package in Cargo.lock.\n\
It is a superset of what any one binary links, including build-only,\n\
development-only and other-platform packages. MIT is selected wherever offered;\n\
additional mandatory licences are retained. Texts and copyright lines are copied\n\
from the crate sources located by `cargo metadata --locked`, including shipped\n\
COPYRIGHT, AUTHORS and NOTICE files.\n\
Regenerate from the repository root with\n\
`AGENT_BRIDGE_UPDATE_NOTICES=1 cargo test --test third_party_notices third_party_notices_match_lock -- --exact`.\n\
The update mode is refused when CI is set and requires Cargo registry sources\n\
(Cargo may download them); normal\n\
tests read only repository files. Unknown licence expressions or missing licence\n\
files require review and an explicit generator update.\n";

fn locked_packages(lock: &str) -> Result<Packages> {
    let lock: toml::Value = toml::from_str(&lock.replace("\r\n", "\n"))?;
    let mut packages = Packages::new();
    for package in lock["package"].as_array().context("Cargo.lock packages")? {
        let name = package["name"].as_str().context("package name")?;
        let version = package["version"].as_str().context("package version")?;
        if name != env!("CARGO_PKG_NAME") {
            ensure!(
                packages.insert((name.to_owned(), version.to_owned())),
                "duplicate lock package: {name} {version}"
            );
        }
    }
    Ok(packages)
}

fn check_notices(lock: &str, notices: &str) -> Result<()> {
    let expected = locked_packages(lock)?;
    let notices = notices.replace("\r\n", "\n");
    let mut actual = Packages::new();
    let mut fenced = false;
    let mut section = None;
    let mut body = Vec::new();
    for line in notices.lines() {
        if line == "```text" || line == "```" {
            fenced = !fenced;
        } else if !fenced && let Some(heading) = line.strip_prefix("## ") {
            if let Some(name) = section {
                check_section(name, &body)?;
            }
            body.clear();
            let (name, version) = heading.split_once(' ').context("package heading")?;
            section = Some(name);
            ensure!(
                actual.insert((name.to_owned(), version.to_owned())),
                "duplicate notice section: {heading}"
            );
            continue;
        }
        if section.is_some() {
            body.push(line);
        }
    }
    if let Some(name) = section {
        check_section(name, &body)?;
    }
    ensure!(!fenced, "unclosed licence text fence");
    ensure!(
        actual == expected,
        "THIRD_PARTY_NOTICES.md differs from Cargo.lock; missing: {:?}; extra: {:?}. \
         Regenerate with AGENT_BRIDGE_UPDATE_NOTICES=1",
        expected.difference(&actual).collect::<Vec<_>>(),
        actual.difference(&expected).collect::<Vec<_>>()
    );
    Ok(())
}

fn check_section(name: &str, lines: &[&str]) -> Result<()> {
    let mut metadata = BTreeSet::new();
    let mut sources = BTreeSet::new();
    let mut pending_source = None;
    let mut active_source = None;
    let mut nonempty = false;
    for &line in lines {
        if let Some(source) = active_source {
            if line == "```" {
                ensure!(nonempty, "empty licence text for {name}: {source}");
                ensure!(
                    sources.insert(source),
                    "duplicate text source for {name}: {source}"
                );
                active_source = None;
            } else {
                nonempty |= !line.trim().is_empty();
            }
        } else if line == "```text" {
            active_source = Some(
                pending_source
                    .take()
                    .context("licence block lacks Text source")?,
            );
            nonempty = false;
        } else if let Some(source) = line.strip_prefix("Text source: ") {
            ensure!(!source.trim().is_empty(), "empty text source for {name}");
            ensure!(
                pending_source.replace(source).is_none(),
                "text source without a body for {name}"
            );
        } else {
            for field in ["Declared licence: ", "Licence used: ", "Repository: "] {
                if let Some(value) = line.strip_prefix(field) {
                    ensure!(!value.trim().is_empty(), "empty {field}for {name}");
                    ensure!(metadata.insert(field), "duplicate {field}for {name}");
                }
            }
        }
    }
    ensure!(metadata.len() == 3, "missing licence metadata for {name}");
    ensure!(
        active_source.is_none() && pending_source.is_none(),
        "unfinished licence text for {name}"
    );
    ensure!(!sources.is_empty(), "missing licence text for {name}");
    if name == "unicode-ident" {
        ensure!(
            sources.contains("LICENSE-MIT") && sources.contains("LICENSE-UNICODE"),
            "unicode-ident requires both LICENSE-MIT and LICENSE-UNICODE texts"
        );
    }
    Ok(())
}

fn update_requested(update: bool, ci: bool) -> Result<bool> {
    ensure!(
        !(update && ci),
        "AGENT_BRIDGE_UPDATE_NOTICES=1 is forbidden when CI is set; regenerate locally"
    );
    Ok(update)
}

fn append_text(output: &mut String, source: &str, text: &str) -> Result<()> {
    let text = text.replace("\r\n", "\n");
    ensure!(!text.trim().is_empty(), "empty licence text: {source}");
    ensure!(
        !text.contains("```"),
        "licence text contains Markdown fence: {source}"
    );
    output.push_str(&format!("\nText source: {source}\n\n```text\n{text}"));
    if !text.ends_with('\n') {
        output.push('\n');
    }
    output.push_str("```\n");
    Ok(())
}

fn generate_notices(root: &Path, lock: &str) -> Result<String> {
    let metadata = Command::new(env!("CARGO"))
        .current_dir(root)
        .args(["metadata", "--locked", "--format-version", "1"])
        .output()
        .context("could not run cargo metadata --locked")?;
    ensure!(
        metadata.status.success(),
        "cargo metadata --locked failed; run it directly for diagnostics"
    );
    let metadata: serde_json::Value = serde_json::from_slice(&metadata.stdout)?;
    let packages = metadata["packages"]
        .as_array()
        .context("metadata packages")?;
    let mut output = HEADER.to_owned();
    for (name, version) in locked_packages(lock)? {
        let package = packages
            .iter()
            .find(|p| p["name"] == name && p["version"] == version)
            .with_context(|| format!("metadata missing {name} {version}"))?;
        let declared = package["license"].as_str().context("declared licence")?;
        let used = match declared {
            "MIT"
            | "MIT OR Apache-2.0"
            | "Apache-2.0 OR MIT"
            | "Unlicense OR MIT"
            | "Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT"
            | "MIT OR Apache-2.0 OR LGPL-2.1-or-later" => "MIT",
            "(MIT OR Apache-2.0) AND Unicode-3.0" => "MIT AND Unicode-3.0",
            _ => bail!("review licence selection for {name} {version}: {declared}"),
        };
        let repository = package["repository"].as_str().context("repository URL")?;
        let directory = Path::new(package["manifest_path"].as_str().context("manifest path")?)
            .parent()
            .context("crate directory")?;
        output.push_str(&format!(
            "\n## {name} {version}\n\nDeclared licence: {declared}\n\n\
             Licence used: {used}\n\nRepository: {repository}\n"
        ));
        // Preserve the shipped spelling even on case-insensitive filesystems.
        let files = fs::read_dir(directory)
            .with_context(|| format!("cannot list licence files for {name} {version}"))?
            .collect::<std::io::Result<Vec<_>>>()?;
        let mit_file = if name == "r-efi" && version == "6.0.0" {
            // This crate ships its complete MIT text and copyright notices in AUTHORS.
            "AUTHORS"
        } else {
            ["LICENSE-MIT", "license-mit"]
                .into_iter()
                .find(|file| files.iter().any(|entry| entry.file_name() == *file))
                .with_context(|| format!("MIT licence file not found for {name} {version}; expected LICENSE-MIT or license-mit; review the crate before updating the generator"))?
        };
        let text = fs::read_to_string(directory.join(mit_file))
            .with_context(|| format!("cannot read {mit_file} for {name} {version}"))?;
        append_text(&mut output, mit_file, &text)?;
        if used == "MIT AND Unicode-3.0" {
            let text =
                fs::read_to_string(directory.join("LICENSE-UNICODE")).with_context(|| {
                    format!("LICENSE-UNICODE not found or unreadable for {name} {version}")
                })?;
            append_text(&mut output, "LICENSE-UNICODE", &text)?;
        }
        let mut additional_files = BTreeSet::new();
        for entry in &files {
            let file = entry.file_name();
            if let Some(file) = file.to_str()
                && file != mit_file
                && ["COPYRIGHT", "AUTHORS", "NOTICE"]
                    .iter()
                    .any(|name| file.eq_ignore_ascii_case(name))
            {
                additional_files.insert(file.to_owned());
            }
        }
        for file in additional_files {
            let text = fs::read_to_string(directory.join(&file))
                .with_context(|| format!("cannot read {file} for {name} {version}"))?;
            append_text(&mut output, &file, &text)?;
        }
    }
    check_notices(lock, &output)?;
    Ok(output)
}

#[test]
fn third_party_notices_match_lock() -> Result<()> {
    let update = update_requested(
        std::env::var("AGENT_BRIDGE_UPDATE_NOTICES").as_deref() == Ok("1"),
        std::env::var_os("CI").is_some(),
    )?;
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let lock = include_str!("../Cargo.lock");
    let notices_path = root.join("THIRD_PARTY_NOTICES.md");
    if update {
        // Finish generation and validation before replacing the existing legal record.
        let generated = generate_notices(root, lock)?;
        fs::write(&notices_path, generated)?;
    }
    check_notices(lock, &fs::read_to_string(notices_path)?)
}

#[test]
fn notices_inventory_rejects_drift_and_accepts_crlf() {
    let lock = "[[package]]\nname = 'dep'\nversion = '1.2.3'\n";
    let notice = "## dep 1.2.3\nDeclared licence: MIT\nLicence used: MIT\nRepository: https://example.com/dep\nText source: LICENSE-MIT\n```text\n## not a package\n```\n";
    assert!(check_notices(lock, notice).is_ok());
    assert!(check_notices(&lock.replace('\n', "\r\n"), &notice.replace('\n', "\r\n")).is_ok());
    assert!(check_notices(lock, "").is_err());
    assert!(check_notices(lock, &notice.replace("1.2.3", "1.2.4")).is_err());
    assert!(check_notices(lock, &format!("{notice}\n## extra 1.0.0\n")).is_err());
    assert!(check_notices(lock, &format!("{notice}{notice}")).is_err());
}

#[test]
fn notices_sections_require_metadata_and_licence_text() {
    let lock = "[[package]]\nname = 'dep'\nversion = '1.2.3'\n";
    let metadata = "## dep 1.2.3\nDeclared licence: MIT\nLicence used: MIT\nRepository: https://example.com/dep\n";
    let body = "Text source: LICENSE-MIT\n```text\npermission text\n```\n";
    let valid = format!("{metadata}{body}");
    assert!(check_notices(lock, &valid).is_ok());
    assert!(check_notices(lock, "## dep 1.2.3\n").is_err());
    assert!(check_notices(lock, metadata).is_err());
    assert!(check_notices(lock, &valid.replace("permission text", "  ")).is_err());
    for field in [
        "Declared licence: MIT\n",
        "Licence used: MIT\n",
        "Repository: https://example.com/dep\n",
    ] {
        assert!(check_notices(lock, &valid.replace(field, "")).is_err());
        let (label, _) = field.split_once(": ").unwrap();
        assert!(check_notices(lock, &valid.replace(field, &format!("{label}: \n"))).is_err());
    }
}

#[test]
fn unicode_notices_require_both_nonempty_licence_texts() {
    let lock = "[[package]]\nname = 'unicode-ident'\nversion = '1.0.24'\n";
    let metadata = "## unicode-ident 1.0.24\nDeclared licence: (MIT OR Apache-2.0) AND Unicode-3.0\nLicence used: MIT AND Unicode-3.0\nRepository: https://github.com/dtolnay/unicode-ident\n";
    let mit = "Text source: LICENSE-MIT\n```text\nMIT text\n```\n";
    let unicode = "Text source: LICENSE-UNICODE\n```text\nUnicode text\n```\n";
    assert!(check_notices(lock, &format!("{metadata}{mit}{unicode}")).is_ok());
    assert!(check_notices(lock, &format!("{metadata}{mit}")).is_err());
    assert!(check_notices(lock, &format!("{metadata}{unicode}")).is_err());
    for text in ["MIT text", "Unicode text"] {
        assert!(
            check_notices(lock, &format!("{metadata}{mit}{unicode}").replace(text, "")).is_err()
        );
    }
}

#[test]
fn notice_updates_are_refused_in_ci() {
    assert!(!update_requested(false, false).unwrap());
    assert!(!update_requested(false, true).unwrap());
    assert!(update_requested(true, false).unwrap());
    assert!(
        update_requested(true, true)
            .unwrap_err()
            .to_string()
            .contains("when CI is set")
    );
}
