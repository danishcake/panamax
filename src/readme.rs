use std::{
    collections::{BTreeMap, HashSet},
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
use url::{Host, Url};

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
    let rendered = read_archive_readme(&archive_path, crate_name, version)?.map(|readme| {
        Arc::<str>::from(render_markdown(
            &readme.contents,
            crate_name,
            version,
            &readme.path,
        ))
    });
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

/// A README and its location within a crate archive.
/// The path is required to allow rewriting of relative links
struct ArchiveReadme {
    /// The README path relative to the crate archive root.
    path: PathBuf,
    /// The README's Markdown contents.
    contents: String,
}

/// Read the README for a given crate. This requires reading Cargo.toml first, the following the
/// readme entry.
fn read_archive_readme(
    archive_path: &Path,
    crate_name: &str,
    version: &str,
) -> io::Result<Option<ArchiveReadme>> {
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
                let relative_path = entry_path
                    .strip_prefix(&root)
                    .map_err(std::io::Error::other)?
                    .to_path_buf();
                return Ok(Some(ArchiveReadme {
                    path: relative_path,
                    contents,
                }));
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
            return Ok(Some(ArchiveReadme {
                path: relative_path.to_path_buf(),
                contents,
            }));
        }
        if best_default
            .as_ref()
            .map(|(best_rank, _)| rank < *best_rank)
            .unwrap_or(true)
        {
            best_default = Some((
                rank,
                ArchiveReadme {
                    path: relative_path.to_path_buf(),
                    contents,
                },
            ));
        }
    }

    Ok(best_default.map(|(_, readme)| readme))
}

/// Read a file from a crate archive using a path relative to the crate root.
pub(crate) fn read_crate_file(
    mirror_path: &Path,
    crate_name: &str,
    version: &str,
    relative_path: &Path,
) -> io::Result<Option<Vec<u8>>> {
    // Crate name and version are untrusted input, so validate before constructing paths
    if !is_safe_path_segment(crate_name) || !is_safe_path_segment(version) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid crate name or version",
        ));
    }

    // Normalize the crate relative path - e.g. removing './', failing on '..'
    let relative_path = normalize_path_within_crate(relative_path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid crate archive path"))?;

    // Determine the path to the crate on disk
    let archive_path = crate::crates::get_crate_path(mirror_path, crate_name, version)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid crate name"))?;

    // A crate archive contains a single folder in the form {name}-{version}
    let target_path = PathBuf::from(format!("{crate_name}-{version}")).join(relative_path);

    // Open the archive and iterate until we find the requested entry
    let mut archive = open_archive(&archive_path)?;

    for entry in archive.entries()? {
        let mut entry = entry?;
        if entry.header().entry_type().is_file() && entry.path()?.as_ref() == target_path.as_path()
        {
            let mut contents = Vec::new();
            entry.read_to_end(&mut contents)?;
            return Ok(Some(contents));
        }
    }
    Ok(None)
}

/// An entry within the crate
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct CrateDirectoryEntry {
    /// The file or directory name
    pub name: String,

    /// If this entry is a directory
    pub is_directory: bool,
}

/// List the direct children of a directory in a crate archive.
pub(crate) fn list_crate_directory(
    mirror_path: &Path,
    crate_name: &str,
    version: &str,
    relative_path: &Path,
) -> io::Result<Option<Vec<CrateDirectoryEntry>>> {
    // Crate name and version are untrusted input, so validate before constructing paths
    if !is_safe_path_segment(crate_name) || !is_safe_path_segment(version) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid crate name or version",
        ));
    }

    // Normalize the crate relative path - e.g. removing './', failing on '..'
    let relative_path = if relative_path.as_os_str().is_empty() {
        PathBuf::new()
    } else {
        normalize_path_within_crate(relative_path).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid crate archive path")
        })?
    };

    // Determine the path to the crate on disk
    let archive_path = crate::crates::get_crate_path(mirror_path, crate_name, version)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid crate name"))?;

    // A crate archive contains a single folder in the form {name}-{version}
    let archive_root = PathBuf::from(format!("{crate_name}-{version}"));
    let target_path = archive_root.join(&relative_path);

    // Open the archive and iterate until we find the requested directory
    // When we find it, iterate through it's children and add them to a BTreeMap,
    // which provides sorting alphabetically
    let mut archive = open_archive(&archive_path)?;
    let mut found_directory = relative_path.as_os_str().is_empty();
    let mut children = BTreeMap::<String, bool>::new();

    for entry in archive.entries()? {
        let entry = entry?;
        let entry_path = entry.path()?.into_owned();
        let entry_type = entry.header().entry_type();
        if !entry_type.is_file() && !entry_type.is_dir() {
            continue;
        }

        // If the entry is the requested directory, track that so we can indicate we found it
        let is_directory = entry_type.is_dir();
        if entry_path == target_path {
            found_directory |= is_directory;
            continue;
        }

        // Extract the names of the directory entries
        let Ok(descendant) = entry_path.strip_prefix(&target_path) else {
            continue;
        };
        let mut components = descendant.components();
        let Some(Component::Normal(child_name)) = components.next() else {
            continue;
        };
        let Some(child_name) = child_name.to_str() else {
            continue;
        };

        // If for some reason we didn't find the directory itself, but have found a child of it,
        // track that we've found the directory
        found_directory = true;

        // Add to list of children
        let child_is_directory = is_directory || components.next().is_some();
        children
            .entry(child_name.to_owned())
            .and_modify(|is_directory| *is_directory |= child_is_directory)
            .or_insert(child_is_directory);
    }

    Ok(found_directory.then(|| {
        children
            .into_iter()
            .map(|(name, is_directory)| CrateDirectoryEntry { name, is_directory })
            .collect()
    }))
}

/// Checks that a path segment is safe. It checks it's not . or .., then ensures it's
/// in [a-zA-Z0-9-_.+]. This should prevent path traversal attacks
fn is_safe_path_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment != "."
        && segment != ".."
        && segment
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_.+".contains(character))
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
            normalize_path_within_crate(Path::new(readme.as_str().unwrap()))
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
fn normalize_path_within_crate(path: &Path) -> Option<PathBuf> {
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

fn render_markdown(markdown: &str, crate_name: &str, version: &str, readme_path: &Path) -> String {
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

    // Rewrite link URLs
    let link_crate_name = crate_name.to_owned();
    let link_version = version.to_owned();
    let link_readme_path = readme_path.to_path_buf();
    options.extension.link_url_rewriter = Some(std::sync::Arc::new(move |url: &str| {
        rewrite_crate_url(url, &link_crate_name, &link_version, &link_readme_path)
    }));

    // Rewrite image URLs
    let image_crate_name = crate_name.to_owned();
    let image_version = version.to_owned();
    let image_readme_path = readme_path.to_path_buf();
    options.extension.image_url_rewriter = Some(std::sync::Arc::new(move |url: &str| {
        rewrite_crate_url(url, &image_crate_name, &image_version, &image_readme_path)
    }));

    // Allow arbitrary HTML
    options.render.r#unsafe = true;

    // Add code block syntax highlightin
    let adapter = SYNTECT_ADAPTER.get_or_init(|| {
        comrak::plugins::syntect::SyntectAdapterBuilder::new()
            .theme("base16-ocean.dark")
            .build()
    });
    let mut plugins = comrak::options::Plugins::default();
    plugins.render.codefence_syntax_highlighter = Some(adapter);

    // Render to HTML
    let html = comrak::markdown_to_html_with_plugins(markdown, &options, &plugins);

    // Sanitize the HTML
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

/// Given a URL within rendered HTML, rewrite it so that it can be resolved as a file within
/// the crate archive
fn rewrite_crate_url(url: &str, crate_name: &str, version: &str, readme_path: &Path) -> String {
    if url.starts_with('#') {
        return url.to_owned();
    }

    // Links to crates.io are resolved to other crates on the mirror
    if let Ok(parsed_url) = Url::parse(url) {
        if let Some(local_crate_url) = rewrite_crates_io_crate_url(&parsed_url) {
            return local_crate_url;
        }
    }
    let mut base = Url::parse("https://panamax.invalid/").expect("static base URL is valid");
    base.set_path(&format!("/{}", readme_path.to_string_lossy()));

    let destination = match Url::parse(url) {
        Ok(url) if is_loopback_url(&url) => {
            let path_and_suffix = format!(
                "{}{}{}",
                url.path(),
                url.query()
                    .map(|query| format!("?{query}"))
                    .unwrap_or_default(),
                url.fragment()
                    .map(|fragment| format!("#{fragment}"))
                    .unwrap_or_default()
            );
            let root = Url::parse("https://panamax.invalid/").expect("static base URL is valid");
            root.join(&path_and_suffix).ok()
        }
        Ok(_) => return url.to_owned(),
        Err(_) if url.starts_with("//") => return url.to_owned(),
        Err(_) => base.join(url).ok(),
    };
    let Some(destination) = destination else {
        return url.to_owned();
    };
    if destination.host_str() != Some("panamax.invalid") {
        return url.to_owned();
    }

    format!(
        "/crate/{crate_name}/{version}/source{}{}{}",
        destination.path(),
        destination
            .query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default(),
        destination
            .fragment()
            .map(|fragment| format!("#{fragment}"))
            .unwrap_or_default()
    )
}

fn rewrite_crates_io_crate_url(url: &Url) -> Option<String> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(url.host_str(), Some("crates.io" | "www.crates.io"))
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return None;
    }

    let mut segments = url.path_segments()?;
    if segments.next()? != "crates" {
        return None;
    }
    let name = segments.next()?;
    if name.is_empty()
        || !name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || "-_".contains(character))
    {
        return None;
    }
    if segments.next().is_some_and(|segment| !segment.is_empty()) || segments.next().is_some() {
        return None;
    }

    Some(format!(
        "/crate/{name}{}{}",
        url.query()
            .map(|query| format!("?{query}"))
            .unwrap_or_default(),
        url.fragment()
            .map(|fragment| format!("#{fragment}"))
            .unwrap_or_default()
    ))
}

fn is_loopback_url(url: &Url) -> bool {
    matches!(url.scheme(), "http" | "https")
        && match url.host() {
            Some(Host::Ipv4(address)) => address.is_loopback(),
            Some(Host::Ipv6(address)) => address.is_loopback(),
            Some(Host::Domain(domain)) => domain == "localhost" || domain.ends_with(".localhost"),
            None => false,
        }
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
                    "# Hello\n\n**World** :balloon:\n\n[Guide](guide.md) ![Logo](logo.png) [Examples](/examples) [Loopback](http://127.0.0.1:8080/examples) [Anchor](#user-content-hello) [External](https://example.com/docs)\n\n```rust\nfn main() {}\n```\n\n<script>alert('unsafe')</script>",
                ),
                ("docs/guide.md", "# Local guide"),
                ("docs/logo.png", "not really a png"),
                ("examples/README.md", "# Examples"),
            ],
        );

        let rendered = render_crate_readme(mirror_path, "foo", "1.0.0")
            .unwrap()
            .unwrap();
        assert!(rendered.contains("<strong>World</strong>"));
        assert!(rendered.contains("🎈"));
        assert!(rendered.contains("href=\"/crate/foo/1.0.0/source/docs/guide.md\""));
        assert!(rendered.contains("src=\"/crate/foo/1.0.0/source/docs/logo.png\""));
        assert_eq!(
            rendered
                .matches("href=\"/crate/foo/1.0.0/source/examples\"")
                .count(),
            2
        );
        assert!(rendered.contains("href=\"#user-content-hello\""));
        assert!(rendered.contains("href=\"https://example.com/docs\""));
        assert!(rendered.contains("style=\"color:"));
        assert!(!rendered.contains("<script>"));
        assert!(!rendered.contains("unsafe"));
    }

    #[test]
    fn rewrites_only_direct_crates_io_crate_links() {
        let readme_path = Path::new("README.md");
        assert_eq!(
            rewrite_crate_url(
                "https://crates.io/crates/atty",
                "current-crate",
                "1.0.0",
                readme_path,
            ),
            "/crate/atty"
        );
        assert_eq!(
            rewrite_crate_url(
                "https://crates.io/crates/atty/?source=readme#details",
                "current-crate",
                "1.0.0",
                readme_path,
            ),
            "/crate/atty?source=readme#details"
        );

        for url in [
            "https://crates.io/crates/atty/0.2.14",
            "https://crates.io/crates/atty/versions",
            "https://crates.io/categories",
            "https://example.com/crates/atty",
        ] {
            assert_eq!(
                rewrite_crate_url(url, "current-crate", "1.0.0", readme_path),
                url
            );
        }
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
    fn reads_crate_files_and_rejects_paths_outside_the_archive() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let mirror_path = temporary_directory.path();
        write_crate_archive(
            mirror_path,
            "foo",
            "1.0.0",
            &[
                ("Cargo.toml", "[package]\nname = \"foo\"\n"),
                ("examples/demo.rs", "fn main() {}"),
            ],
        );

        assert_eq!(
            read_crate_file(mirror_path, "foo", "1.0.0", Path::new("examples/demo.rs"))
                .unwrap()
                .unwrap(),
            b"fn main() {}"
        );
        assert_eq!(
            read_crate_file(mirror_path, "foo", "1.0.0", Path::new("missing")).unwrap(),
            None
        );
        assert_eq!(
            read_crate_file(mirror_path, "foo", "1.0.0", Path::new("../Cargo.toml"))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(
            list_crate_directory(mirror_path, "foo", "1.0.0", Path::new("examples"))
                .unwrap()
                .unwrap(),
            vec![CrateDirectoryEntry {
                name: "demo.rs".to_owned(),
                is_directory: false,
            }]
        );
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
