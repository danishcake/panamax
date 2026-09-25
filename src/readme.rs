use std::{
    borrow::Cow,
    collections::{HashMap, VecDeque},
    fs::File,
    io::{self, Read},
    path::{Component, Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use ammonia::{Builder, UrlRelative, UrlRelativeEvaluate};
use flate2::read::GzDecoder;
use tar::Archive;
use url::Url;

use crate::crates::get_crate_path;

const README_CACHE_CAPACITY: usize = 100;
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

#[derive(Default)]
struct ReadmeCache {
    rendered: HashMap<PathBuf, Option<Arc<str>>>,
    recency: VecDeque<PathBuf>,
}

impl ReadmeCache {
    fn get(&mut self, key: &Path) -> Option<Option<Arc<str>>> {
        let rendered = self.rendered.get(key)?.clone();
        self.touch(key);
        Some(rendered)
    }

    fn insert(&mut self, key: PathBuf, rendered: Option<Arc<str>>) {
        if self.rendered.contains_key(&key) {
            self.rendered.insert(key.clone(), rendered);
            self.touch(&key);
            return;
        }
        self.remove_from_recency(&key);
        while self.rendered.len() >= README_CACHE_CAPACITY {
            let Some(oldest) = self.recency.pop_front() else {
                break;
            };
            self.rendered.remove(&oldest);
        }
        self.recency.push_back(key.clone());
        self.rendered.insert(key, rendered);
    }

    fn touch(&mut self, key: &Path) {
        self.remove_from_recency(key);
        self.recency.push_back(key.to_path_buf());
    }

    fn remove_from_recency(&mut self, key: &Path) {
        if let Some(position) = self.recency.iter().position(|entry| entry == key) {
            self.recency.remove(position);
        }
    }
}

static README_CACHE: OnceLock<Mutex<ReadmeCache>> = OnceLock::new();

/// Read and render the README from a local crate archive, caching the latest 100 results.
pub fn render_crate_readme(
    mirror_path: &Path,
    crate_name: &str,
    version: &str,
) -> io::Result<Option<Arc<str>>> {
    let archive_path = get_crate_path(mirror_path, crate_name, version)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid crate name"))?;

    if let Some(rendered) = cache_get(&archive_path) {
        return Ok(rendered);
    }

    let rendered = read_archive_readme(&archive_path, crate_name, version)?.map(|readme| {
        Arc::<str>::from(render_markdown(
            &readme.contents,
            &readme.path,
            readme.repository.as_deref(),
        ))
    });
    cache_insert(archive_path, rendered.clone());
    Ok(rendered)
}

fn cache_get(key: &Path) -> Option<Option<Arc<str>>> {
    let cache = README_CACHE.get_or_init(|| Mutex::new(ReadmeCache::default()));
    cache.lock().ok()?.get(key)
}

fn cache_insert(key: PathBuf, rendered: Option<Arc<str>>) {
    let cache = README_CACHE.get_or_init(|| Mutex::new(ReadmeCache::default()));
    if let Ok(mut cache) = cache.lock() {
        cache.insert(key, rendered);
    }
}

struct CrateReadme {
    path: PathBuf,
    contents: String,
    repository: Option<String>,
}

enum ReadmeLocation {
    Explicit(PathBuf),
    Default,
    Disabled,
}

struct ReadmeManifest {
    location: ReadmeLocation,
    repository: Option<String>,
}

fn read_archive_readme(
    archive_path: &Path,
    crate_name: &str,
    version: &str,
) -> io::Result<Option<CrateReadme>> {
    let root = PathBuf::from(format!("{crate_name}-{version}"));
    let Some(manifest) = read_archive_file(archive_path, &root.join("Cargo.toml"))? else {
        return Ok(None);
    };
    let manifest = readme_manifest(&manifest)?;
    if matches!(&manifest.location, ReadmeLocation::Disabled) {
        return Ok(None);
    }

    let Some(mut archive) = open_archive(archive_path)? else {
        return Ok(None);
    };
    let explicit_path = match &manifest.location {
        ReadmeLocation::Explicit(path) => Some(root.join(path)),
        ReadmeLocation::Default | ReadmeLocation::Disabled => None,
    };
    let mut best_default = None;

    for entry in archive.entries()? {
        let mut entry = entry?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let entry_path = entry.path()?.into_owned();

        if let Some(expected_path) = explicit_path.as_ref() {
            if &entry_path == expected_path {
                let mut contents = String::new();
                entry.read_to_string(&mut contents)?;
                return Ok(Some(CrateReadme {
                    path: match &manifest.location {
                        ReadmeLocation::Explicit(path) => path.clone(),
                        ReadmeLocation::Default | ReadmeLocation::Disabled => unreachable!(),
                    },
                    contents,
                    repository: manifest.repository.clone(),
                }));
            }
            continue;
        }

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
        let mut contents = String::new();
        entry.read_to_string(&mut contents)?;
        if rank == 0 {
            return Ok(Some(CrateReadme {
                path: relative_path.to_path_buf(),
                contents,
                repository: manifest.repository.clone(),
            }));
        }
        if best_default
            .as_ref()
            .map(|(best_rank, _, _)| rank < *best_rank)
            .unwrap_or(true)
        {
            best_default = Some((rank, relative_path.to_path_buf(), contents));
        }
    }

    Ok(best_default.map(|(_, path, contents)| CrateReadme {
        path,
        contents,
        repository: manifest.repository,
    }))
}

fn open_archive(archive_path: &Path) -> io::Result<Option<Archive<GzDecoder<File>>>> {
    match File::open(archive_path) {
        Ok(file) => Ok(Some(Archive::new(GzDecoder::new(file)))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn read_archive_file(archive_path: &Path, target_path: &Path) -> io::Result<Option<String>> {
    let Some(mut archive) = open_archive(archive_path)? else {
        return Ok(None);
    };
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

fn readme_manifest(manifest: &str) -> io::Result<ReadmeManifest> {
    let manifest = manifest
        .parse::<toml_edit::Document>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
    let Some(package) = manifest
        .get("package")
        .or_else(|| manifest.get("project"))
        .and_then(|item| item.as_table())
    else {
        return Ok(ReadmeManifest {
            location: ReadmeLocation::Disabled,
            repository: None,
        });
    };

    let location = match package.get("readme") {
        Some(readme) if readme.as_bool() == Some(false) => ReadmeLocation::Disabled,
        Some(readme) if readme.as_str().is_some() => {
            normalize_readme_path(Path::new(readme.as_str().unwrap()))
                .map(ReadmeLocation::Explicit)
                .unwrap_or(ReadmeLocation::Disabled)
        }
        _ => ReadmeLocation::Default,
    };
    let repository = package
        .get("repository")
        .and_then(|item| item.as_str())
        .map(str::to_owned);
    Ok(ReadmeManifest {
        location,
        repository,
    })
}

fn normalize_readme_path(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(part) => normalized.push(part),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    (!normalized.as_os_str().is_empty()).then_some(normalized)
}

fn default_readme_rank(filename: &str) -> Option<usize> {
    match filename.to_ascii_lowercase().as_str() {
        "readme.md" => Some(0),
        "readme.markdown" => Some(1),
        "readme.txt" => Some(2),
        "readme" => Some(3),
        _ => None,
    }
}

fn render_markdown(markdown: &str, readme_path: &Path, repository: Option<&str>) -> String {
    let mut options = comrak::Options::default();
    options.extension.alerts = true;
    options.extension.autolink = true;
    options.extension.description_lists = true;
    options.extension.multiline_block_quotes = true;
    options.extension.strikethrough = true;
    options.extension.table = true;
    options.extension.tasklist = true;
    options.extension.footnotes = true;
    options.extension.header_id_prefix = Some("user-content-".to_string());
    options.extension.header_id_prefix_in_href = true;
    options.render.r#unsafe = true;
    let html = comrak::markdown_to_html(markdown, &options);

    let mut sanitizer = Builder::default();
    sanitizer
        .add_tags(&["input", "ol", "picture", "section", "source"])
        .link_rel(Some("nofollow noopener noreferrer"))
        .add_generic_attributes(&["align"])
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
        .id_prefix(Some("user-content-"));
    sanitizer.add_allowed_classes("code", CODE_CLASSES);

    let relative_urls = repository
        .and_then(repository_base_url)
        .map(|base_url| {
            UrlRelative::Custom(Box::new(ReadmeUrlResolver {
                base_url,
                base_dir: readme_path
                    .parent()
                    .and_then(Path::to_str)
                    .unwrap_or_default()
                    .to_owned(),
            }))
        })
        .unwrap_or(UrlRelative::Deny);
    sanitizer.url_relative(relative_urls);
    sanitizer.clean(&html).to_string()
}

fn repository_base_url(repository: &str) -> Option<String> {
    let mut url = Url::parse(repository).ok()?;
    if !matches!(
        url.host_str()?,
        "github.com" | "gitlab.com" | "bitbucket.org"
    ) {
        return None;
    }
    url.set_query(None);
    url.set_fragment(None);
    let mut path = url.path().trim_end_matches('/').to_owned();
    if path.ends_with(".git") {
        path.truncate(path.len() - 4);
    }
    url.set_path(&format!("{path}/"));
    Some(url.into())
}

struct ReadmeUrlResolver {
    base_url: String,
    base_dir: String,
}

impl UrlRelativeEvaluate<'_> for ReadmeUrlResolver {
    fn evaluate<'url>(&self, url: &'url str) -> Option<Cow<'url, str>> {
        if url.starts_with('#') {
            return Some(Cow::Borrowed(url));
        }
        if url.starts_with("::") {
            return None;
        }

        let url_path = url.split(['?', '#']).next().unwrap_or(url);
        let extension = Path::new(url_path)
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase);
        let is_media = matches!(
            extension.as_deref(),
            Some("svg" | "png" | "jpg" | "jpeg" | "gif" | "mp4" | "webm" | "ogg" | "webp")
        );
        let mut resolved = self.base_url.clone();
        resolved.push_str(if is_media { "raw/HEAD" } else { "blob/HEAD" });

        let mut path = String::new();
        if !self.base_dir.is_empty() {
            path.push_str(&self.base_dir);
            path.push('/');
        }
        path.push_str(url.strip_prefix('/').unwrap_or(url));
        let (path, suffix) = match path.find(['?', '#']) {
            Some(index) => path.split_at(index),
            None => (path.as_str(), ""),
        };
        resolved.push('/');
        resolved.push_str(&normalize_path(path));
        resolved.push_str(suffix);

        if is_media && extension.as_deref() == Some("svg") {
            if let Ok(mut parsed_url) = Url::parse(&resolved) {
                parsed_url.query_pairs_mut().append_pair("sanitize", "true");
                resolved = parsed_url.into();
            }
        }
        Some(Cow::Owned(resolved))
    }
}

fn normalize_path(path: &str) -> String {
    let mut segments = Vec::new();
    for segment in path.split('/') {
        match segment {
            "." => {}
            ".." => {
                segments.pop();
            }
            segment => segments.push(segment),
        }
    }
    segments.join("/")
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
                    "[package]\nname = \"foo\"\nversion = \"1.0.0\"\nreadme = \"docs/INTRO.md\"\nrepository = \"https://github.com/example/foo.git\"\n",
                ),
                (
                    "docs/INTRO.md",
                    "# Hello\n\n**World**\n\n[Guide](guide.md) ![Logo](logo.png)\n\n<script>alert('unsafe')</script>",
                ),
            ],
        );

        let rendered = render_crate_readme(mirror_path, "foo", "1.0.0")
            .unwrap()
            .unwrap();
        assert!(rendered.contains("<strong>World</strong>"));
        assert!(
            rendered.contains("href=\"https://github.com/example/foo/blob/HEAD/docs/guide.md\"")
        );
        assert!(rendered.contains("src=\"https://github.com/example/foo/raw/HEAD/docs/logo.png\""));
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
        let mut cache = ReadmeCache::default();
        for index in 0..README_CACHE_CAPACITY {
            cache.insert(
                PathBuf::from(format!("crate-{index}")),
                Some(Arc::from(format!("rendered-{index}"))),
            );
        }
        assert!(cache.get(Path::new("crate-0")).is_some());
        cache.insert(PathBuf::from("crate-100"), Some(Arc::from("rendered-100")));

        assert!(cache.get(Path::new("crate-0")).is_some());
        assert!(cache.get(Path::new("crate-1")).is_none());
        assert_eq!(cache.rendered.len(), README_CACHE_CAPACITY);
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
