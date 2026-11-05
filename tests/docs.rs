//! The documentation promises things the compiler cannot see: that the guide shows the example as it
//! is, and that the links between the README and the docs lead somewhere.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(path: &str) -> String {
    std::fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// docs/WRITING_A_PROVIDER.md section 5 is the whole of examples/cube_provider.rs, so the example
/// the CI builds and tests is the one a reader copies.
#[test]
fn the_guide_shows_the_cube_example_as_it_is() {
    let guide = read("docs/WRITING_A_PROVIDER.md");
    let example = read("examples/cube_provider.rs");
    assert!(
        guide.contains(example.trim_end()),
        "section 5 of docs/WRITING_A_PROVIDER.md differs from examples/cube_provider.rs: \
         paste the example into the guide again"
    );
}

/// The anchors GitHub gives the headings of a Markdown file: lower case, spaces to hyphens,
/// punctuation dropped.
fn anchors(text: &str) -> Vec<String> {
    let mut in_code = false;
    let mut out = Vec::new();
    for line in text.lines() {
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
        }
        let hashes = line.chars().take_while(|&c| c == '#').count();
        if in_code || hashes == 0 || !line[hashes..].starts_with(' ') {
            continue;
        }
        out.push(
            line[hashes..]
                .trim()
                .to_lowercase()
                .chars()
                .filter(|c| c.is_alphanumeric() || *c == ' ' || *c == '-' || *c == '_')
                .map(|c| if c == ' ' { '-' } else { c })
                .collect(),
        );
    }
    out
}

/// Every relative link of the Markdown files points at a file or folder that exists, and a link to
/// a heading points at a heading that exists. The Bevy plugin is a separate package of the workspace and
/// is not in the package of this crate (see `Cargo.toml`), so where it is absent, as in the crate
/// downloaded from crates.io, its README and the links that lead into it are left out.
#[test]
fn relative_links_resolve() {
    let with_bevy = root().join("rewrite-bridge-bevy").exists();
    for file in [
        "README.md",
        "docs/WRITING_A_PROVIDER.md",
        "docs/CONTRACT.md",
        "docs/INTEGRATING.md",
        "rewrite-bridge-bevy/README.md",
    ] {
        if !with_bevy && file.starts_with("rewrite-bridge-bevy") {
            continue;
        }
        let text = read(file);
        let dir = Path::new(file).parent().unwrap();
        let mut in_code = false;
        for line in text.lines() {
            if line.trim_start().starts_with("```") {
                in_code = !in_code;
            }
            if in_code {
                continue;
            }
            // `[text](target)`; a target with a scheme is external and not checked here
            for part in line.split("](").skip(1) {
                let Some(target) = part.split(')').next() else {
                    continue;
                };
                if target.contains("://") || target.starts_with("mailto:") {
                    continue;
                }
                let (path, anchor) = target.split_once('#').unwrap_or((target, ""));
                if !with_bevy && path.contains("rewrite-bridge-bevy") {
                    continue;
                }
                let full = if path.is_empty() {
                    root().join(file)
                } else {
                    root().join(dir).join(path)
                };
                assert!(full.exists(), "{file}: the link ({target}) leads nowhere");
                if !anchor.is_empty() && full.extension().is_some_and(|e| e == "md") {
                    let headings = anchors(&std::fs::read_to_string(&full).unwrap());
                    assert!(
                        headings.iter().any(|h| h == anchor),
                        "{file}: the link ({target}) names a heading that does not exist"
                    );
                }
            }
        }
    }
}

/// An example `spawn` in the docs that names the Warframe provider shows an id that provider lists: the path of
/// an `.fbx` model or a `.level` (with the `?animations=` or `?collision` suffix if it has one), never a
/// `uniqueName` such as `/Lotus/Powersuits/Excalibur/Excalibur`, which `Warframe::load` refuses. A reader copies the
/// examples, so a wrong one is copied.
#[test]
fn warframe_examples_use_asset_ids_the_provider_lists() {
    let mut seen = 0;
    for file in [
        "docs/CONTRACT.md",
        "docs/INTEGRATING.md",
        "docs/WRITING_A_PROVIDER.md",
        "README.md",
    ] {
        for line in read(file)
            .lines()
            .filter(|l| l.contains("\"provider\":\"Warframe\""))
        {
            let asset = line
                .split("\"asset\":\"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap_or_else(|| panic!("{file}: no asset in `{line}`"));
            let path = asset.split('?').next().unwrap();
            assert!(
                path.starts_with("/Lotus/") && (path.ends_with(".fbx") || path.ends_with(".level")),
                "{file}: the example asset {asset:?} is not the path of a model or level (wf-unpack lists those)"
            );
            seen += 1;
        }
    }
    assert!(
        seen > 0,
        "no Warframe example spawn found; the test looks at the wrong place"
    );
}
