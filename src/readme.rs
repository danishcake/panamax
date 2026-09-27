use std::{
    collections::HashSet,
    fs::File,
    io::{self, Read},
    num::NonZeroUsize,
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use ammonia::Builder;
use flate2::read::GzDecoder;
use lru::LruCache;
use tar::Archive;

use crate::crates::get_crate_path;

/// The number of rendered READMEs to keep cached
const README_CACHE_CAPACITY: usize = 100;

/// A set of CSS classes related to rendered code. Given most code will
/// be Rust, this is probably overkill
const CODE_CLASSES: &[&str] = &[
    "language-bash",
    "language-c",
    "language-cpp",
    "language-glsl",
    "language-go",
    "language-ini",
    "language-javascript",
    "language-json",
    "language-markdown",
    "language-protobuf",
    "language-python",
    "language-ruby",
    "language-rust",
    "language-scss",
    "language-sql",
    "language-toml",
    "language-typescript",
    "language-xml",
    "language-yaml",
    "language-clike",
    "language-rs",
    "language-markup",
];

type ReadmeCache = Mutex<LruCache<PathBuf, Option<Arc<str>>>>;

/// The cache of previously rendered READMEs
static README_CACHE: OnceLock<ReadmeCache> = OnceLock::new();
static SYNTECT_ADAPTER: OnceLock<comrak::plugins::syntect::SyntectAdapter> = OnceLock::new();

/// Read and render the README from a local crate archive, caching the latest 100 results.
pub fn render_crate_readme(
    mirror_path: &Path,
    crate_name: &str,
    version: &str,
) -> io::Result<Option<Arc<str>>> {
    // Determine path to the crate
    let archive_path = get_crate_path(mirror_path, crate_name, version)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid crate name"))?;

    // Return the cached rendering if possible
    if let Some(rendered) = cache_get(&archive_path) {
        return Ok(rendered);
    }

    // Otherwise render and put into the cache
    let rendered = read_archive_readme(&archive_path, crate_name, version)?
        .map(|readme| Arc::<str>::from(render_markdown(&readme)));
    cache_insert(archive_path, rendered.clone());
    Ok(rendered)
}

fn cache_get(key: &Path) -> Option<Option<Arc<str>>> {
    let cache = readme_cache();
    cache.lock().ok()?.get(key).cloned()
}

fn cache_insert(key: PathBuf, rendered: Option<Arc<str>>) {
    let cache = readme_cache();
    if let Ok(mut cache) = cache.lock() {
        cache.put(key, rendered);
    }
}

/// Gets the cache. If it hasn't been initialized, initializes it first
fn readme_cache() -> &'static ReadmeCache {
    README_CACHE.get_or_init(|| {
        Mutex::new(LruCache::new(
            NonZeroUsize::new(README_CACHE_CAPACITY)
                .expect("README cache capacity must be nonzero"),
        ))
    })
}

/// Where the Cargo.toml indicates that a README should be located
enum ReadmeLocation {
    /// An explicit path is used if the [package] or [project] readme key contains a string
    Explicit(PathBuf),
    /// The default path is used if not readme key is present, or is set to true
    Default,
    /// No readme is present if the readme key is set to fals
    NotPresent,
}

/// Read the README for a given crate. This requires reading Cargo.toml first, the following the
/// readme entry.
fn read_archive_readme(
    archive_path: &Path,
    crate_name: &str,
    version: &str,
) -> io::Result<Option<String>> {
    let root = PathBuf::from(format!("{crate_name}-{version}"));
    let Some(manifest) = read_archive_file_as_string(archive_path, &root.join("Cargo.toml"))?
    else {
        return Ok(None);
    };

    // Determine the README file from Cargo.toml
    let readme_location = determine_readme_location(&manifest)?;
    if matches!(&readme_location, ReadmeLocation::NotPresent) {
        return Ok(None);
    }

    // Open the crate so we can iterate through the entries
    let mut archive = open_archive(archive_path)?;
    let explicit_path = match &readme_location {
        ReadmeLocation::Explicit(path) => Some(root.join(path)),
        ReadmeLocation::Default | ReadmeLocation::NotPresent => None,
    };
    let mut best_default = None;

    // Iterate through the entries, and find the best README
    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let entry_path = entry.path()?.into_owned();

        // An exact match on the explicit path is an automatic best match
        if let Some(expected_path) = explicit_path.as_ref() {
            if &entry_path == expected_path {
                let mut contents = String::new();
                entry.read_to_string(&mut contents)?;
                return Ok(Some(contents));
            }

            // If there is an explicit path, only accept exact matches
            continue;
        }

        // Manipulate the entry path by stripping the root, checking that the resulting entry is not nested,
        // mapping the filename to a Rust string and determining the 'rank' of the readme
        let Ok(relative_path) = entry_path.strip_prefix(&root) else {
            continue;
        };
        if relative_path.components().count() != 1 {
            continue;
        }
        let Some(filename) = relative_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(rank) = default_readme_rank(filename) else {
            continue;
        };

        // Read the README. If it's best option return early
        // Otherwise store if it's a higher rank
        let mut contents = String::new();
        entry.read_to_string(&mut contents)?;
        if rank == 0 {
            return Ok(Some( contents ));
        }
        if best_default
            .as_ref()
            .map(|(best_rank, _)| rank < *best_rank)
            .unwrap_or(true)
        {
            best_default = Some((rank, contents));
        }
    }

    Ok(best_default.map(|(_, contents)| contents ))
}

/// Opens the archive and returns an iterable Archive on success
fn open_archive(archive_path: &Path) -> io::Result<Archive<GzDecoder<File>>> {
    match File::open(archive_path) {
        Ok(file) => Ok(Archive::new(GzDecoder::new(file))),
        Err(error) => Err(error),
    }
}

/// Reads a file from the archive
/// If not found, Ok(None) is returned
fn read_archive_file_as_string(
    archive_path: &Path,
    target_path: &Path,
) -> io::Result<Option<String>> {
    let mut archive = open_archive(archive_path)?;

    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_file() && entry.path()?.as_ref() == target_path {
            let mut contents = String::new();
            entry.read_to_string(&mut contents)?;
            return Ok(Some(contents));
        }
    }
    Ok(None)
}

/// Given a Cargo.toml, determine the path to the readme
fn determine_readme_location(manifest: &str) -> io::Result<ReadmeLocation> {
    // Parse the toml
    let manifest = manifest
        .parse::<toml_edit::Document>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;

    // Find the 'package' key, or the older 'project' key
    let Some(package) = manifest
        .get("package")
        .or_else(|| manifest.get("project"))
        .and_then(|item| item.as_table())
    else {
        // If neither is present then the package is malformed - this should be vanishing rare
        // and/or impossible, as crates.io would reject a malformed package
        return Ok(ReadmeLocation::NotPresent);
    };

    // The readme key can be a bool, whereupon true indicates README.md
    // and false disables the readme.
    // If not present, the default location is used
    let location = match package.get("readme") {
        Some(readme) if readme.as_bool() == Some(false) => ReadmeLocation::NotPresent,
        Some(readme) if readme.as_str().is_some() => {
            normalize_readme_path(Path::new(readme.as_str().unwrap()))
                .map(ReadmeLocation::Explicit)
                .unwrap_or(ReadmeLocation::NotPresent)
        }
        _ => ReadmeLocation::Default,
    };
    Ok(location)
}

/// Transforms the readme path into a standard form.
/// Any '.' part is skipped
/// Any '..' part is considered an error, and results in None
fn normalize_readme_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    // An empty result is transformed to None
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

/// If a non-explicit README is present, prioritise the one with the lowest rank
fn default_readme_rank(filename: &str) -> Option<usize> {
    match filename.to_ascii_lowercase().as_str() {
        "readme.md" => Some(0),
        "readme.markdown" => Some(1),
        "readme.txt" => Some(2),
        "readme" => Some(3),
        _ => None,
    }
}

fn render_markdown(markdown: &str) -> String {
    let mut options = comrak::Options::default();
    options.extension.alerts = true;
    options.extension.autolink = true;
    options.extension.description_lists = true;
    options.extension.multiline_block_quotes = true;
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.tasklist = true;
    options.extension.footnotes = true;
    options.extension.shortcodes = true;
    options.extension.header_id_prefix = Some("user-content-".to_string());
    options.extension.header_id_prefix_in_href = true;
    options.render.r#unsafe = true;
    let adapter = SYNTECT_ADAPTER.get_or_init(|| {
        comrak::plugins::syntect::SyntectAdapterBuilder::new()
            .theme("base16-ocean.dark")
            .build()
    });
    let mut plugins = comrak::options::Plugins::default();
    plugins.render.codefence_syntax_highlighter = Some(adapter);
    let html = comrak::markdown_to_html_with_plugins(markdown, &options, &plugins);

    let mut sanitizer = Builder::default();
    sanitizer
        .add_tags(&["input", "ol", "picture", "section", "source"])
        .link_rel(Some("nofollow noopener noreferrer"))
        .add_generic_attributes(&["align", "style"])
        .add_tag_attributes("a", &["aria-label", "id", "target"])
        .add_tag_attributes("h1", &["id"])
        .add_tag_attributes("h2", &["id"])
        .add_tag_attributes("h3", &["id"])
        .add_tag_attributes("h4", &["id"])
        .add_tag_attributes("h5", &["id"])
        .add_tag_attributes("h6", &["id"])
        .add_tag_attributes("input", &["checked", "disabled", "type"])
        .add_tag_attributes("li", &["id"])
        .add_tag_attributes("source", &["media", "srcset"])
        .add_allowed_classes("section", &["footnotes"])
        .add_allowed_classes(
            "div",
            &[
                "markdown-alert",
                "markdown-alert-note",
                "markdown-alert-tip",
                "markdown-alert-important",
                "markdown-alert-warning",
                "markdown-alert-caution",
            ],
        )
        .add_allowed_classes("p", &["markdown-alert-title"])
        .filter_style_properties(
            ["color", "background-color"]
                .into_iter()
                .collect::<HashSet<_>>(),
        )
        .id_prefix(Some("user-content-"));
    sanitizer.add_allowed_classes("code", CODE_CLASSES);

    sanitizer.clean(&html).to_string()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::Write;

    use flate2::{write::GzEncoder, Compression};
    use tar::{Builder as TarBuilder, Header};

    use super::*;

    #[test]
    fn renders_manifest_selected_readme_and_sanitizes_html() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let mirror_path = temporary_directory.path();
        write_crate_archive(
            mirror_path,
            "foo",
            "1.0.0",
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"foo\"\nversion = \"1.0.0\"\nreadme = \"docs/INTRO.md\"\nrepository = \"https://codeberg.org/example/foo.git\"\n",
                ),
                (
                    "docs/INTRO.md",
                    "# Hello\n\n**World** :balloon:\n\n[Guide](guide.md) ![Logo](logo.png)\n\n```rust\nfn main() {}\n```\n\n<script>alert('unsafe')</script>",
                ),
            ],
        );

        let rendered = render_crate_readme(mirror_path, "foo", "1.0.0")
            .unwrap()
            .unwrap();
        assert!(rendered.contains("<strong>World</strong>"));
        assert!(rendered.contains("🎈"));
        assert!(
            rendered.contains("href=\"https://codeberg.org/example/foo/blob/HEAD/docs/guide.md\"")
        );
        assert!(
            rendered.contains("src=\"https://codeberg.org/example/foo/raw/HEAD/docs/logo.png\"")
        );
        assert!(rendered.contains("style=\"color:"));
        assert!(!rendered.contains("<script>"));
        assert!(!rendered.contains("unsafe"));
    }

    #[test]
    fn reads_the_default_readme_file() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let mirror_path = temporary_directory.path();
        write_crate_archive(
            mirror_path,
            "default-readme",
            "1.0.0",
            &[
                (
                    "Cargo.toml",
                    "[package]\nname = \"default-readme\"\nversion = \"1.0.0\"\n",
                ),
                ("README.md", "# The default README"),
            ],
        );

        let rendered = render_crate_readme(mirror_path, "default-readme", "1.0.0")
            .unwrap()
            .unwrap();
        assert!(rendered.contains("The default README"));
    }

    #[test]
    fn readme_cache_keeps_the_100_most_recent_entries() {
        let mut cache: LruCache<PathBuf, Option<Arc<str>>> =
            LruCache::new(NonZeroUsize::new(README_CACHE_CAPACITY).unwrap());
        for index in 0..README_CACHE_CAPACITY {
            cache.put(
                PathBuf::from(format!("crate-{index}")),
                Some(Arc::from(format!("rendered-{index}"))),
            );
        }
        assert!(cache.get(Path::new("crate-0")).is_some());
        cache.put(PathBuf::from("crate-100"), Some(Arc::from("rendered-100")));

        assert!(cache.get(Path::new("crate-0")).is_some());
        assert!(cache.get(Path::new("crate-1")).is_none());
        assert_eq!(cache.len(), README_CACHE_CAPACITY);
    }

    fn write_crate_archive(mirror_path: &Path, name: &str, version: &str, files: &[(&str, &str)]) {
        let crate_path = get_crate_path(mirror_path, name, version).unwrap();
        fs::create_dir_all(crate_path.parent().unwrap()).unwrap();
        let file = File::create(crate_path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut archive = TarBuilder::new(encoder);
        for (relative_path, contents) in files {
            let mut header = Header::new_gnu();
            header
                .set_path(format!("{name}-{version}/{relative_path}"))
                .unwrap();
            header.set_size(contents.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            archive.append(&header, contents.as_bytes()).unwrap();
        }
        archive
            .into_inner()
            .unwrap()
            .finish()
            .unwrap()
            .flush()
            .unwrap();
    }
}
