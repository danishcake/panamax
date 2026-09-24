use std::{
    collections::HashMap,
    fs::{read_dir, File},
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::Mutex,
};

use flate2::read::GzDecoder;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use semver::Version;
use serde::{Deserialize, Serialize};
use tar::Archive;
use thiserror::Error;
use walkdir::WalkDir;

use crate::crates::get_crate_path;

/// Name of the database stored at the root of a mirror.
pub const DATABASE_FILENAME: &str = "search.db";

/// The database scheme version
const DATABASE_SCHEMA_VERSION: i32 = 2;

/// Metadata key containing the crates.io-index Git commit used to build the database.
const INDEX_GIT_HASH_METADATA_KEY: &str = "index_git_hash";

/// If not specified, the number of results to return per page
const DEFAULT_PER_PAGE: usize = 10;

/// The maximum number of results to return in a single page. This matches
/// the maximum supported by cargo search
const MAX_PER_PAGE: usize = 100;

/// Errors raised while building or querying the local search database.
#[derive(Debug, Error)]
pub enum SearchError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),

    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    #[error("Git error: {0}")]
    Git(#[from] git2::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Invalid crates.io index entry in {path} at line {line}: {source}")]
    IndexEntry {
        path: PathBuf,
        line: usize,
        source: serde_json::Error,
    },

    #[error("Invalid crate version `{version}` for `{crate_name}` in {path}: {source}")]
    CrateVersion {
        path: PathBuf,
        crate_name: String,
        version: String,
        source: semver::Error,
    },

    #[error("Invalid search database: {0}")]
    InvalidDatabase(String),
}

/// Summary of a completed search-index build.
#[derive(Debug, Default, Eq, PartialEq)]
pub struct BuildStats {
    /// Number of crate records written to the database.
    pub crates: usize,
}

#[derive(Debug, Deserialize)]
struct IndexEntry {
    /// The name of the crate
    name: String,
    /// The version of the crate
    vers: String,
    /// If the entry has been yanked
    #[serde(default)]
    yanked: bool,
    /// Dependencies recorded in the crates.io index.
    #[serde(default)]
    deps: Vec<IndexDependency>,
}

/// A dependency of a crate, extracted from the index rather than a crate
#[derive(Debug, Clone, Deserialize)]
struct IndexDependency {
    /// Name of the dependency
    name: String,

    /// The version
    req: String,

    /// Manifest section containing the dependency
    #[serde(default)]
    kind: String,
}

#[derive(Debug, Clone)]
struct VersionEntry {
    version: Version,
    yanked: bool,
}

/// The data about a crate
#[derive(Debug)]
struct CrateRecord {
    /// The name of the crate
    name: String,

    /// The latest version of the crate in the index. If panamax is synced from a
    /// vendored directory it may not be available to download
    latest_version: Option<String>,

    /// The latest yanked version of the crate in the index
    latest_yanked_version: Option<String>,

    /// The latest version of the crate that is actually available
    latest_available_version: Option<String>,

    /// The full list of versions of the crate that are available
    available_versions: Vec<AvailableVersion>,

    /// Metadata about a crate
    metadata: CrateMetadata,
}

#[derive(Debug, Clone, Deserialize, Eq, PartialEq, Serialize)]
pub struct AvailableVersion {
    /// Version present as a local crate archive.
    pub version: String,
    /// Whether this version is yanked in the crates.io index.
    pub yanked: bool,
}

#[derive(Debug, Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Dependency {
    /// Dependency name.
    pub name: String,
    /// Cargo requirement or manifest declaration for the dependency.
    pub requirement: String,
    /// Manifest section containing the dependency.
    pub kind: String,
}

#[derive(Debug, Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CrateMetadata {
    /// The description of the crate
    pub description: Option<String>,

    /// The authors of the crate
    pub authors: Vec<String>,

    /// The license of the crate
    pub license: Option<String>,

    /// The source repository of the crate
    pub repository: Option<String>,

    /// The homepage of the crate
    pub homepage: Option<String>,

    /// The documentation URL
    pub documentation: Option<String>,

    /// A set of keywords to associate with the crate
    pub keywords: Vec<String>,

    /// The categories the crate belongs to
    pub categories: Vec<String>,

    /// The dependencies of the crate
    pub dependencies: Vec<Dependency>,
}

impl CrateMetadata {
    /// Return the metadata fields that should be included in crate search.
    fn searchable_fields(&self) -> (String, String) {
        (self.keywords.join(" "), self.categories.join(" "))
    }

    /// Whether a metadata URL is an absolute HTTP or HTTPS URL suitable for linking.
    pub fn is_http_url(&self, value: &str) -> bool {
        url::Url::parse(value)
            .map(|url| matches!(url.scheme(), "http" | "https"))
            .unwrap_or(false)
    }
}

/// Read-only access to a mirror's prebuilt search database.
pub struct SearchIndex {
    connection: Mutex<Connection>,
}

/// Details stored for one crate in the local search database.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct CrateDetails {
    /// Crate name.
    pub name: String,
    /// Latest non-yanked version, or Cargo's `0.0.0` fallback.
    pub max_version: String,
    /// Latest yanked version, if one exists.
    pub latest_yanked_version: Option<String>,
    /// Latest version whose archive is available in the mirror.
    pub latest_available_version: Option<String>,
    /// Every crate archive available in the mirror.
    pub available_versions: Vec<AvailableVersion>,
    /// Manifest and index metadata for the crate.
    pub metadata: CrateMetadata,
}

impl SearchIndex {
    /// Open and validate the database without creating or modifying it.
    pub fn open(mirror_path: &Path) -> Result<Self, SearchError> {
        // Open the database. If it doesn't exist, the operation fails
        let database_path = mirror_path.join(DATABASE_FILENAME);
        if !database_path.is_file() {
            return Err(SearchError::InvalidDatabase(format!(
                "database does not exist: {}",
                database_path.display()
            )));
        }
        let connection =
            Connection::open_with_flags(&database_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;

        // Reject databases from an incompatible future schema instead of returning bad data.
        let schema_version: i32 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if schema_version != DATABASE_SCHEMA_VERSION {
            return Err(SearchError::InvalidDatabase(format!(
                "unsupported schema version {schema_version}"
            )));
        }

        let stored_git_hash: Option<String> = connection
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                [INDEX_GIT_HASH_METADATA_KEY],
                |row| row.get(0),
            )
            .optional()?;
        let current_git_hash = index_git_hash(mirror_path)?;
        match stored_git_hash {
            Some(stored_git_hash) if stored_git_hash == current_git_hash => {}
            Some(stored_git_hash) => {
                log::warn!(
                    "Search database at `{}` was built from crates.io-index commit {}, but the current commit is {}. Rebuild it with `panamax create-search-index {}`.",
                    database_path.display(),
                    stored_git_hash,
                    current_git_hash,
                    mirror_path.display(),
                );
            }
            None => {
                log::warn!(
                    "Search database at `{}` has no recorded crates.io-index Git commit. Rebuild it with `panamax create-search-index {}`.",
                    database_path.display(),
                    mirror_path.display(),
                );
            }
        }

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// Search the database using Cargo's page-size conventions.
    pub fn search(
        &self,
        query: &str,
        per_page: Option<usize>,
    ) -> Result<SearchResults, SearchError> {
        self.search_page(query, per_page, 1)
    }

    /// Search one 1-based page of results. Cargo's search endpoint uses `search()` so its
    /// existing first-page behavior remains unchanged.
    pub fn search_page(
        &self,
        query: &str,
        per_page: Option<usize>,
        page: usize,
    ) -> Result<SearchResults, SearchError> {
        // Clamp caller-provided values so an HTTP client cannot request an unbounded page.
        let per_page = per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE);
        let connection = self.connection.lock().map_err(|_| {
            SearchError::InvalidDatabase("search database lock was poisoned".into())
        })?;

        let query = query.trim();
        if query.is_empty() {
            // TODO explain
            query_database(&connection, None, query, per_page, page)
        } else if query.chars().count() < 3 {
            // FTS5's trigram tokenizer cannot index one- or two-character terms.
            let pattern = like_literal_pattern(query);
            query_database(&connection, Some(&pattern), query, per_page, page)
        } else {
            // Quote the user input so FTS5 syntax cannot be injected through the query string.
            let fts_query = fts_literal_query(query);
            query_database(&connection, Some(&fts_query), query, per_page, page)
        }
    }

    /// Look up a crate by name without matching descriptions or dependencies.
    pub fn crate_details(&self, name: &str) -> Result<Option<CrateDetails>, SearchError> {
        let connection = self.connection.lock().map_err(|_| {
            SearchError::InvalidDatabase("search database lock was poisoned".into())
        })?;
        let row = connection
            .query_row(
                "SELECT name, latest_version, latest_yanked_version, latest_available_version,
                        available_versions, details
                 FROM crates WHERE lower(name) = lower(?1)",
                [name],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Option<String>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            name,
            latest_version,
            latest_yanked_version,
            latest_available_version,
            available_versions,
            details,
        )) = row
        else {
            return Ok(None);
        };
        let metadata: CrateMetadata = serde_json::from_str(&details)?;
        let available_versions: Vec<AvailableVersion> = serde_json::from_str(&available_versions)?;

        Ok(Some(CrateDetails {
            name,
            max_version: latest_version.unwrap_or_else(|| "0.0.0".to_string()),
            latest_yanked_version,
            latest_available_version,
            available_versions,
            metadata,
        }))
    }
}

/// Return a LIKE pattern that treats the input as literal text.
fn like_literal_pattern(query: &str) -> String {
    // Escape the escape character first so the later wildcard escapes remain unambiguous.
    let query = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    format!("%{query}%")
}

/// Encode input as a full text search phrase.
fn fts_literal_query(query: &str) -> String {
    // Replace any double quotes with a a pair (" -> "")
    // then wrap the whole in double quotes
    format!("\"{}\"", query.replace('"', "\"\""))
}

/// Build or rebuild the search database from local mirror files only.
pub fn build(mirror_path: &Path) -> Result<BuildStats, SearchError> {
    // Validate folder structure
    if !mirror_path.is_dir() {
        return Err(SearchError::InvalidDatabase(format!(
            "mirror directory does not exist: {}",
            mirror_path.display()
        )));
    }

    let index_path = mirror_path.join("crates.io-index");
    if !index_path.is_dir() {
        return Err(SearchError::InvalidDatabase(format!(
            "crates.io-index directory does not exist: {}",
            index_path.display()
        )));
    }
    let index_git_hash = index_git_hash(mirror_path)?;

    // Open the database
    let database_path = mirror_path.join(DATABASE_FILENAME);
    let connection = Connection::open(database_path)?;

    // Use rollback journaling. This keeps the database as a single file
    connection.pragma_update(None, "journal_mode", "delete")?;
    initialize_schema(&connection)?;

    // Update the database in one massive transaction. This should reduce the number
    // of small fsyncs, and keep building this reasonably snappy
    let transaction = connection.unchecked_transaction()?;

    // Clear the existing database
    transaction.execute("DELETE FROM crate_search", [])?;
    transaction.execute("DELETE FROM crates", [])?;

    // Creates prepared statements to insert rows
    let mut crate_insert = transaction.prepare(
        "INSERT INTO crates (
            name, description, latest_version, latest_yanked_version,
            latest_available_version, available_versions, details,
            search_keywords, search_categories
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )?;
    let mut search_insert = transaction.prepare(
        "INSERT INTO crate_search (crate_id, name, description, keywords, categories)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;

    // Track the number of crates indexed
    let mut stats = BuildStats::default();

    // Process one index file at a time. The index has one file per crate, so this
    // keeps memory bounded by the largest individual crate entry and description.
    for entry in WalkDir::new(&index_path).into_iter().filter_entry(|entry| {
        // Don't iterate into dotfolders
        if entry.file_type().is_dir() {
            !entry.file_name().to_string_lossy().starts_with('.')
        } else {
            // Don't yield files in the top level
            entry.depth() > 1
        }
    }) {
        let entry = entry.map_err(|error| io::Error::other(error.to_string()))?;
        if !entry.file_type().is_file() {
            continue;
        }

        // Read name, latest version and description
        let path = entry.into_path();
        let Some(record) = read_index_file(mirror_path, &path)? else {
            continue;
        };

        // Add to the database
        let details = serde_json::to_string(&record.metadata)?;
        let (keywords, categories) = record.metadata.searchable_fields();
        let available_versions = serde_json::to_string(&record.available_versions)?;
        crate_insert.execute(params![
            record.name,
            record.metadata.description,
            record.latest_version,
            record.latest_yanked_version,
            record.latest_available_version,
            available_versions,
            details,
            keywords,
            categories,
        ])?;

        // The FTS table stores the row ID so query results can recover full crate metadata.
        // We can't use a proper foreign key constraint here
        let crate_id = transaction.last_insert_rowid();
        search_insert.execute(params![
            crate_id,
            record.name,
            record.metadata.description,
            keywords,
            categories,
        ])?;
        stats.crates += 1;
    }

    // Drop the prepared statements so that the borrows of transaction finish
    drop(search_insert);
    drop(crate_insert);

    // Keep build metadata available for diagnostics without affecting search results.
    transaction.execute(
        "INSERT INTO metadata (key, value) VALUES ('generated_at', strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        [],
    )?;
    transaction.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![INDEX_GIT_HASH_METADATA_KEY, index_git_hash],
    )?;
    transaction.commit()?;

    Ok(stats)
}

/// Returns the commit currently checked out in the local crates.io-index repository.
fn index_git_hash(mirror_path: &Path) -> Result<String, SearchError> {
    let repository = git2::Repository::open(mirror_path.join("crates.io-index"))?;
    let hash = repository.head()?.peel_to_commit()?.id().to_string();
    Ok(hash)
}

/// Initializes the database
fn initialize_schema(connection: &Connection) -> Result<(), SearchError> {
    let schema_version: i32 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if schema_version != DATABASE_SCHEMA_VERSION {
        // Building replaces every row, so recreating the schema is safe for both
        // older databases and databases from a newer incompatible build.
        connection.execute_batch(
            "DROP TABLE IF EXISTS crate_search;
             DROP TABLE IF EXISTS crates;
             DROP TABLE IF EXISTS metadata;",
        )?;
    }

    // The trigram tokenizer provides indexed substring matching for names and descriptions.
    connection.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS crates (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE COLLATE NOCASE,
            description TEXT,
            latest_version TEXT,
            latest_yanked_version TEXT,
            latest_available_version TEXT,
            available_versions TEXT NOT NULL,
            details TEXT NOT NULL,
            search_keywords TEXT NOT NULL,
            search_categories TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS crates_name_idx ON crates(name COLLATE NOCASE);
        CREATE VIRTUAL TABLE IF NOT EXISTS crate_search USING fts5(
            crate_id UNINDEXED,
            name,
            description,
            keywords,
            categories,
            tokenize = 'trigram'
        );
        CREATE TABLE IF NOT EXISTS metadata (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        PRAGMA user_version = {DATABASE_SCHEMA_VERSION};"
    ))?;
    Ok(())
}

/// Reads a crate from the index
/// An index file consists of a series of JSON objects, one per line. Each line represents
/// a published version of the crate. Expanded, they look like this:
///
/// {
///   "name":"a",
///   "vers":"0.1.0",
///   "deps":[
///     {"name":"base64","req":"^0.22","features":[],"optional":true,"default_features":true,"target":null,"kind":"normal"},
///     {"name":"chrono","req":"^0.4","features":["serde"],"optional":false,"default_features":true,"target":null,"kind":"normal"},
///     {"name":"serde","req":"^1","features":["derive"],"optional":true,"default_features":true,"target":null,"kind":"normal"},
///     {"name":"sm4","req":"^0.5","features":[],"optional":true,"default_features":true,"target":null,"kind":"normal"},
///     {"name":"thiserror","req":"^2","features":[],"optional":false,"default_features":true,"target":null,"kind":"normal"}
///   ],
///   "cksum":"58169884a52f5647006cc1d663adb1432a948a8af1a28379880274d0b7a160e8",
///   "features":{"default":[]},
///   "features2":{"serde":["dep:serde","chrono/serde"],
///   "sm4":["dep:sm4","dep:base64"]},
///   "yanked":false,
///   "pubtime":"2026-01-15T12:53:12Z",
///   "v":2
/// }
///
/// This data is suplemented by reading metadata from the latest version of the crate available
fn read_index_file(mirror_path: &Path, path: &Path) -> Result<Option<CrateRecord>, SearchError> {
    let file = BufReader::new(File::open(path)?);
    let mut crate_name = None;
    let mut latest_version = None;
    let mut latest_yanked_version = None;
    let mut versions = Vec::new();
    let mut latest_index_dependencies = Vec::new();

    // Each index file contains all published versions for one crate.
    for (line_number, line) in file.lines().enumerate() {
        let line_number = line_number + 1;
        let line = line?;
        let index_entry: IndexEntry =
            serde_json::from_str(&line).map_err(|source| SearchError::IndexEntry {
                path: path.to_path_buf(),
                line: line_number,
                source,
            })?;
        let version =
            Version::parse(&index_entry.vers).map_err(|source| SearchError::CrateVersion {
                path: path.to_path_buf(),
                crate_name: index_entry.name.clone(),
                version: index_entry.vers.clone(),
                source,
            })?;
        crate_name.get_or_insert(index_entry.name);

        let candidate = VersionEntry {
            version,
            yanked: index_entry.yanked,
        };
        versions.push(candidate.clone());
        let index_dependencies = index_entry
            .deps
            .iter()
            .map(|dependency| Dependency {
                name: dependency.name.clone(),
                requirement: dependency.req.clone(),
                kind: dependency.kind.clone(),
            })
            .collect::<Vec<_>>();
        // Keep only the two candidates needed by the search response.
        let latest = if candidate.yanked {
            &mut latest_yanked_version
        } else {
            &mut latest_version
        };
        if latest
            .as_ref()
            .map(|current: &VersionEntry| candidate.version > current.version)
            .unwrap_or(true)
        {
            *latest = Some(candidate);
            if !index_entry.yanked {
                latest_index_dependencies = index_dependencies;
            }
        }
    }

    // If the crate name could not be identified then return None.
    let Some(crate_name) = crate_name else {
        return Ok(None);
    };
    let available_versions = local_versions(mirror_path, &crate_name, &versions)?;

    // Metadata is read from the downloaded crates
    // Identify the latest version available, prioritising non-yanked
    let mut latest_available_version = available_versions
        .iter()
        .find(|entry| !entry.yanked)
        .map(|entry| entry.version.clone());
    if latest_available_version.is_none() {
        latest_available_version = available_versions
            .first()
            .map(|entry| entry.version.clone());
    }

    // Work through the available versions from latest to oldest. Attempt to extract
    // metadata from each version, terminating as soon as one succeeds
    let mut metadata = CrateMetadata::default();
    for entry in &available_versions {
        match read_manifest_metadata(mirror_path, &crate_name, &entry.version) {
            Ok(Some(candidate_metadata)) => {
                metadata = candidate_metadata;
                break;
            }
            Ok(None) => {}
            Err(error) => {
                log::warn!(
                    "Could not read metadata for {crate_name} {}: {error}",
                    entry.version
                );
            }
        }
    }
    if metadata.dependencies.is_empty() {
        metadata.dependencies = latest_index_dependencies;
    }

    Ok(Some(CrateRecord {
        name: crate_name,
        latest_version: latest_version.map(|entry| entry.version.to_string()),
        latest_yanked_version: latest_yanked_version.map(|entry| entry.version.to_string()),
        latest_available_version,
        available_versions,
        metadata,
    }))
}

/// Lists crate archives that are actually present in the mirror.
fn local_versions(
    mirror_path: &Path,
    crate_name: &str,
    index_versions: &[VersionEntry],
) -> Result<Vec<AvailableVersion>, SearchError> {
    let sample_path = get_crate_path(mirror_path, crate_name, "0.0.0")
        .ok_or_else(|| io::Error::other(format!("invalid crate name: {crate_name}")))?;
    let crate_directory = sample_path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::other("invalid crate archive path"))?;
    let yanked_versions = index_versions
        .iter()
        .map(|entry| (entry.version.to_string(), entry.yanked))
        .collect::<HashMap<_, _>>();

    let entries = match read_dir(crate_directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut available_versions = Vec::new();
    for entry in entries {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let version = entry.file_name().to_string_lossy().into_owned();
        let Ok(parsed_version) = Version::parse(&version) else {
            continue;
        };
        let archive = entry.path().join(format!("{crate_name}-{version}.crate"));
        if archive.is_file() {
            available_versions.push((
                parsed_version,
                yanked_versions.get(&version).copied().unwrap_or(false),
            ));
        }
    }
    available_versions.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| b.0.cmp(&a.0)));
    Ok(available_versions
        .into_iter()
        .map(|(version, yanked)| AvailableVersion {
            version: version.to_string(),
            yanked,
        })
        .collect())
}

/// Reads searchable and displayable metadata for a given crate from the Cargo.toml file.
/// This data is only available for downloaded crates
fn read_manifest_metadata(
    mirror_path: &Path,
    crate_name: &str,
    version: &str,
) -> Result<Option<CrateMetadata>, SearchError> {
    // Selective mirrors may not contain the archive, which is a valid no-description case.
    let crate_path = get_crate_path(mirror_path, crate_name, version)
        .ok_or_else(|| io::Error::other(format!("invalid crate name: {crate_name}")))?;
    let file = match File::open(crate_path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };

    // Read the compressed archive as a stream; only Cargo.toml is retained in memory.
    let decoder = GzDecoder::new(file);
    let mut archive = Archive::new(decoder);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let entry_path = entry.path()?.to_path_buf();
        let expected_root = format!("{crate_name}-{version}");
        // Avoid accidentally using a nested Cargo.toml from an unusual archive.
        let is_manifest = entry_path
            .file_name()
            .map(|name| name.eq_ignore_ascii_case("Cargo.toml"))
            .unwrap_or(false)
            && entry_path
                .parent()
                .and_then(Path::file_name)
                .map(|name| name == std::ffi::OsStr::new(&expected_root))
                .unwrap_or(false);
        if !is_manifest {
            continue;
        }

        let mut contents = String::new();
        entry.read_to_string(&mut contents)?;
        let manifest = contents
            .parse::<toml_edit::Document>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        let Some(package) = manifest
            .get("package")
            .or_else(|| manifest.get("project"))
            .and_then(|item| item.as_table())
        else {
            return Ok(Some(CrateMetadata::default()));
        };

        let string_field = |name: &str| {
            package
                .get(name)
                .and_then(|item| item.as_str())
                .map(str::to_owned)
        };
        let array_field = |name: &str| {
            package
                .get(name)
                .and_then(|item| item.as_array())
                .map(|array| {
                    array
                        .iter()
                        .filter_map(|value| value.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };

        let mut dependencies = Vec::new();
        for kind in ["dependencies", "dev-dependencies", "build-dependencies"] {
            if let Some(table) = manifest.get(kind).and_then(|item| item.as_table()) {
                dependencies.extend(table.iter().map(|(name, value)| Dependency {
                    name: name.to_string(),
                    requirement: value.to_string(),
                    kind: kind.to_string(),
                }));
            }
        }

        return Ok(Some(CrateMetadata {
            description: string_field("description"),
            authors: array_field("authors"),
            license: string_field("license"),
            repository: string_field("repository"),
            homepage: string_field("homepage"),
            documentation: string_field("documentation"),
            keywords: array_field("keywords"),
            categories: array_field("categories"),
            dependencies,
        }));
    }

    Ok(Some(CrateMetadata::default()))
}

/// One crate returned by a search query.
#[derive(Debug, Eq, PartialEq)]
pub struct SearchResult {
    /// Crate name.
    pub name: String,
    /// Description from the selected local crate archive, if available.
    pub description: Option<String>,
    /// Latest non-yanked version, or Cargo's `0.0.0` fallback.
    pub max_version: String,
}

/// Search results and the total number of matches before pagination.
#[derive(Debug, Eq, PartialEq)]
pub struct SearchResults {
    /// Number of matching crates before the page limit is applied.
    pub total: usize,
    /// Current 1-based page, clamped to the available range.
    pub page: usize,
    /// Number of result pages (at least one, including when there are no matches).
    pub total_pages: usize,
    /// Crates returned for the requested page.
    pub crates: Vec<SearchResult>,
}

fn query_database(
    connection: &Connection,
    search_query: Option<&str>,
    display_query: &str,
    per_page: usize,
    requested_page: usize,
) -> Result<SearchResults, SearchError> {
    // Short terms use LIKE because trigram FTS cannot represent them; longer terms use FTS.
    let (where_clause, bind_query) = match search_query {
        Some(_) if display_query.chars().count() < 3 => (
            "WHERE lower(c.name) LIKE lower(?1) ESCAPE '\\'
                OR lower(COALESCE(c.description, '')) LIKE lower(?1) ESCAPE '\\'
                OR lower(c.search_keywords) LIKE lower(?1) ESCAPE '\\'
                OR lower(c.search_categories) LIKE lower(?1) ESCAPE '\\'",
            search_query,
        ),
        Some(_) => (
            "WHERE c.id IN (SELECT crate_id FROM crate_search WHERE crate_search MATCH ?1)",
            search_query,
        ),
        None => ("", None),
    };

    let count_sql = format!(
        "SELECT COUNT(*) FROM crates c
         {where_clause}"
    );
    // Cargo expects the total count to describe all matches, not just the returned page.
    let total: i64 = match bind_query {
        Some(query) => connection.query_row(&count_sql, [query], |row| row.get(0))?,
        None => connection.query_row(&count_sql, [], |row| row.get(0))?,
    };
    let total = usize::try_from(total).unwrap_or(usize::MAX);
    let total_pages = (total / per_page + usize::from(total % per_page != 0)).max(1);
    let page = requested_page.clamp(1, total_pages);
    let offset = (page - 1) * per_page;

    let query_sql = format!(
        "SELECT c.name, c.description, COALESCE(c.latest_version, '0.0.0')
         FROM crates c
         {where_clause}
         ORDER BY CASE WHEN lower(c.name) = lower(?2) THEN 0
                       WHEN lower(c.name) LIKE lower(?3) THEN 1
                       ELSE 2 END,
                  c.name COLLATE NOCASE
          LIMIT ?4 OFFSET ?5"
    );
    let name_pattern = like_literal_pattern(display_query);
    // The SQL text contains only fixed clauses; every user value is bound as a parameter.
    let mut statement = connection.prepare(&query_sql)?;
    let mut rows = match bind_query {
        Some(query) => statement.query(params![
            query,
            display_query,
            name_pattern,
            per_page,
            offset
        ])?,
        None => statement.query(params!["", display_query, name_pattern, per_page, offset])?,
    };
    let mut crates = Vec::new();
    while let Some(row) = rows.next()? {
        crates.push(SearchResult {
            name: row.get(0)?,
            description: row.get(1)?,
            max_version: row.get(2)?,
        });
    }

    Ok(SearchResults {
        total,
        page,
        total_pages,
        crates,
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write};

    use flate2::{write::GzEncoder, Compression};
    use tar::Builder;

    use super::*;

    #[test]
    fn builds_and_queries_an_offline_index() {
        // Build a minimal mirror fixture without any network access.
        let temporary_directory = tempfile::tempdir().unwrap();
        let mirror_path = temporary_directory.path();
        fs::create_dir_all(mirror_path.join("crates.io-index/3/f")).unwrap();
        fs::create_dir_all(mirror_path.join("crates/3/f/foo/1.2.0")).unwrap();
        fs::write(
            mirror_path.join("crates.io-index/3/f/foo"),
            concat!(
                "{\"name\":\"foo\",\"vers\":\"1.0.0\",\"yanked\":false}\n",
                "{\"name\":\"foo\",\"vers\":\"1.2.0\",\"yanked\":false}\n"
            ),
        )
        .unwrap();
        write_crate_archive(mirror_path, "foo", "1.2.0", "A useful foo crate.");
        let git_hash = create_git_repo(mirror_path);

        let stats = build(mirror_path).unwrap();
        assert_eq!(stats.crates, 1);
        assert!(!mirror_path.join("search.db-journal").exists());
        assert!(!mirror_path.join("search.db-wal").exists());

        let connection = Connection::open(mirror_path.join(DATABASE_FILENAME)).unwrap();
        let stored_git_hash: String = connection
            .query_row(
                "SELECT value FROM metadata WHERE key = ?1",
                [INDEX_GIT_HASH_METADATA_KEY],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored_git_hash, git_hash);

        let index = SearchIndex::open(mirror_path).unwrap();
        let results = index.search("useful", None).unwrap();
        assert_eq!(results.total, 1);
        assert_eq!(results.crates[0].name, "foo");
        assert_eq!(results.crates[0].max_version, "1.2.0");
        assert_eq!(
            results.crates[0].description.as_deref(),
            Some("A useful foo crate.")
        );

        // SQL quotes, FTS operators, and LIKE wildcards must remain ordinary search text.
        assert_eq!(index.search("' OR 1=1 --", None).unwrap().total, 0);
        assert_eq!(index.search("foo OR *", None).unwrap().total, 0);
        assert_eq!(index.search("%", None).unwrap().total, 0);
    }

    #[test]
    fn rebuild_replaces_a_future_schema() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let mirror_path = temporary_directory.path();
        fs::create_dir_all(mirror_path.join("crates.io-index/3/f")).unwrap();
        fs::write(
            mirror_path.join("crates.io-index/3/f/foo"),
            "{\"name\":\"foo\",\"vers\":\"1.0.0\",\"yanked\":false}\n",
        )
        .unwrap();
        create_git_repo(mirror_path);
        build(mirror_path).unwrap();

        let connection = Connection::open(mirror_path.join(DATABASE_FILENAME)).unwrap();
        connection
            .pragma_update(None, "user_version", DATABASE_SCHEMA_VERSION + 1)
            .unwrap();
        drop(connection);

        build(mirror_path).unwrap();
        assert!(SearchIndex::open(mirror_path).is_ok());
    }

    #[test]
    fn pagination_reports_total_matches() {
        // Multiple files verify that the directory walk streams each crate independently.
        let temporary_directory = tempfile::tempdir().unwrap();
        let mirror_path = temporary_directory.path();
        fs::create_dir_all(mirror_path.join("crates.io-index/3/f")).unwrap();
        for name in ["foo", "food"] {
            fs::write(
                mirror_path.join(format!("crates.io-index/3/f/{name}")),
                format!("{{\"name\":\"{name}\",\"vers\":\"1.0.0\",\"yanked\":false}}\n"),
            )
            .unwrap();
        }
        create_git_repo(mirror_path);
        build(mirror_path).unwrap();

        let index = SearchIndex::open(mirror_path).unwrap();
        let results = index.search("foo", Some(1)).unwrap();
        assert_eq!(results.total, 2);
        assert_eq!(results.page, 1);
        assert_eq!(results.total_pages, 2);
        assert_eq!(results.crates.len(), 1);
        assert_eq!(results.crates[0].name, "foo");

        let second_page = index.search_page("foo", Some(1), 2).unwrap();
        assert_eq!(second_page.page, 2);
        assert_eq!(second_page.total_pages, 2);
        assert_eq!(second_page.crates.len(), 1);
        assert_eq!(second_page.crates[0].name, "food");

        let out_of_range = index.search_page("foo", Some(1), usize::MAX).unwrap();
        assert_eq!(out_of_range.page, 2);
        assert_eq!(out_of_range.crates[0].name, "food");
    }

    #[test]
    fn reads_crate_details_from_manifest() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let mirror_path = temporary_directory.path();
        fs::create_dir_all(mirror_path.join("crates.io-index/3/f")).unwrap();
        fs::create_dir_all(mirror_path.join("crates/3/f/foo/1.2.0")).unwrap();
        fs::write(
            mirror_path.join("crates.io-index/3/f/foo"),
            "{\"name\":\"foo\",\"vers\":\"1.2.0\",\"yanked\":false}\n",
        )
        .unwrap();
        write_manifest_archive(
            mirror_path,
            "foo",
            "1.2.0",
            "[package]\nname = \"foo\"\nversion = \"1.2.0\"\ndescription = \"A useful foo crate.\"\nauthors = [\"A. Author\"]\nlicense = \"MIT\"\nrepository = \"https://example.com/foo\"\nkeywords = [\"example\"]\ncategories = [\"encoding\"]\n\n[dependencies]\nserde = \"1\"\n",
        );
        create_git_repo(mirror_path);
        build(mirror_path).unwrap();

        let index = SearchIndex::open(mirror_path).unwrap();
        let details = index.crate_details("FOO").unwrap().unwrap();
        assert_eq!(details.name, "foo");
        assert_eq!(
            details.metadata.description.as_deref(),
            Some("A useful foo crate.")
        );
        assert_eq!(details.latest_available_version.as_deref(), Some("1.2.0"));
        assert_eq!(
            details.available_versions,
            vec![AvailableVersion {
                version: "1.2.0".to_string(),
                yanked: false,
            }]
        );
        assert_eq!(details.metadata.authors, vec!["A. Author"]);
        assert_eq!(details.metadata.license.as_deref(), Some("MIT"));
        assert_eq!(
            details.metadata.repository.as_deref(),
            Some("https://example.com/foo")
        );
        assert_eq!(details.metadata.keywords, vec!["example"]);
        assert_eq!(details.metadata.categories, vec!["encoding"]);
        assert_eq!(details.metadata.dependencies.len(), 1);
        assert_eq!(details.metadata.dependencies[0].name, "serde");
        assert_eq!(details.metadata.dependencies[0].kind, "dependencies");
        assert_eq!(index.search("example", None).unwrap().total, 1);
        assert_eq!(index.search("en", None).unwrap().total, 1);
        assert_eq!(index.search("MIT", None).unwrap().total, 0);
        assert_eq!(index.search("Author", None).unwrap().total, 0);
    }

    #[test]
    fn uses_newest_available_archive_for_details() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let mirror_path = temporary_directory.path();
        fs::create_dir_all(mirror_path.join("crates.io-index/3/f")).unwrap();
        fs::create_dir_all(mirror_path.join("crates/3/f/foo/1.0.0")).unwrap();
        fs::create_dir_all(mirror_path.join("crates/3/f/foo/1.5.0")).unwrap();
        fs::write(
            mirror_path.join("crates.io-index/3/f/foo"),
            concat!(
                "{\"name\":\"foo\",\"vers\":\"1.0.0\",\"yanked\":false}\n",
                "{\"name\":\"foo\",\"vers\":\"1.5.0\",\"yanked\":false}\n",
                "{\"name\":\"foo\",\"vers\":\"2.0.0\",\"yanked\":false}\n"
            ),
        )
        .unwrap();
        write_crate_archive(mirror_path, "foo", "1.0.0", "Available foo crate.");
        write_crate_archive(mirror_path, "foo", "1.5.0", "Newer available foo crate.");
        create_git_repo(mirror_path);
        build(mirror_path).unwrap();

        let index = SearchIndex::open(mirror_path).unwrap();
        let details = index.crate_details("foo").unwrap().unwrap();
        assert_eq!(details.max_version, "2.0.0");
        assert_eq!(details.latest_available_version.as_deref(), Some("1.5.0"));
        assert_eq!(details.available_versions.len(), 2);
        assert_eq!(details.available_versions[0].version, "1.5.0");
        assert_eq!(details.available_versions[1].version, "1.0.0");
        assert_eq!(
            details.metadata.description.as_deref(),
            Some("Newer available foo crate.")
        );
    }

    #[test]
    fn uses_index_dependencies_when_archive_is_missing() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let mirror_path = temporary_directory.path();
        fs::create_dir_all(mirror_path.join("crates.io-index/3/a")).unwrap();
        fs::write(
            mirror_path.join("crates.io-index/3/a/abstract-testing"),
            concat!(
                "{\"name\":\"abstract-testing\",\"vers\":\"0.26.1\",\"deps\":[",
                "{\"name\":\"serde\",\"req\":\"^1.0\",\"kind\":\"normal\"}],",
                "\"yanked\":false}\n"
            ),
        )
        .unwrap();
        create_git_repo(mirror_path);
        build(mirror_path).unwrap();

        let index = SearchIndex::open(mirror_path).unwrap();
        let details = index.crate_details("abstract-testing").unwrap().unwrap();
        assert!(details.latest_available_version.is_none());
        assert_eq!(details.available_versions, Vec::new());
        assert_eq!(details.metadata.dependencies.len(), 1);
        assert_eq!(details.metadata.dependencies[0].name, "serde");
    }

    #[test]
    fn opens_stale_index_with_warning() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let mirror_path = temporary_directory.path();
        fs::create_dir_all(mirror_path.join("crates.io-index/3/f")).unwrap();
        fs::write(
            mirror_path.join("crates.io-index/3/f/foo"),
            "{\"name\":\"foo\",\"vers\":\"1.0.0\",\"yanked\":false}\n",
        )
        .unwrap();
        create_git_repo(mirror_path);
        build(mirror_path).unwrap();

        fs::write(
            mirror_path.join("crates.io-index/3/f/foo"),
            concat!(
                "{\"name\":\"foo\",\"vers\":\"1.0.0\",\"yanked\":false}\n",
                "{\"name\":\"foo\",\"vers\":\"2.0.0\",\"yanked\":false}\n"
            ),
        )
        .unwrap();
        create_git_repo(mirror_path);

        // A changed index is warned about but remains available until explicitly rebuilt.
        let index = SearchIndex::open(mirror_path).unwrap();
        let results = index.search("foo", None).unwrap();
        assert_eq!(results.total, 1);
        assert_eq!(results.crates[0].max_version, "1.0.0");
    }

    #[test]
    fn opens_database_without_git_hash_with_warning() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let mirror_path = temporary_directory.path();
        fs::create_dir_all(mirror_path.join("crates.io-index/3/f")).unwrap();
        fs::write(
            mirror_path.join("crates.io-index/3/f/foo"),
            "{\"name\":\"foo\",\"vers\":\"1.0.0\",\"yanked\":false}\n",
        )
        .unwrap();
        create_git_repo(mirror_path);
        build(mirror_path).unwrap();

        let connection = Connection::open(mirror_path.join(DATABASE_FILENAME)).unwrap();
        connection
            .execute(
                "DELETE FROM metadata WHERE key = ?1",
                [INDEX_GIT_HASH_METADATA_KEY],
            )
            .unwrap();

        assert!(SearchIndex::open(mirror_path).is_ok());
    }

    /// Creates a git repo in `mirror_path` and return the commit hash
    fn create_git_repo(mirror_path: &Path) -> String {
        let repository = git2::Repository::init(mirror_path.join("crates.io-index")).unwrap();
        let mut index = repository.index().unwrap();
        index
            .add_all(["*"].iter(), git2::IndexAddOption::DEFAULT, None)
            .unwrap();
        index.write().unwrap();
        let tree = repository.find_tree(index.write_tree().unwrap()).unwrap();
        let signature = git2::Signature::now("Panamax test", "test@panamax").unwrap();
        let parent = repository
            .head()
            .ok()
            .and_then(|head| head.peel_to_commit().ok());
        let parents = parent.as_ref().into_iter().collect::<Vec<_>>();
        let commit = repository
            .commit(
                Some("HEAD"),
                &signature,
                &signature,
                "Update index",
                &tree,
                &parents,
            )
            .unwrap();
        commit.to_string()
    }

    fn write_crate_archive(mirror_path: &Path, name: &str, version: &str, description: &str) {
        let manifest = format!(
            "[package]\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"{description}\"\n"
        );
        write_manifest_archive(mirror_path, name, version, &manifest);
    }

    fn write_manifest_archive(mirror_path: &Path, name: &str, version: &str, manifest: &str) {
        let crate_path = get_crate_path(mirror_path, name, version).unwrap();
        let file = File::create(crate_path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut archive = Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header
            .set_path(format!("{name}-{version}/Cargo.toml"))
            .unwrap();
        header.set_size(manifest.len() as u64);
        header.set_mode(0o644);
        header.set_cksum();
        archive.append(&header, manifest.as_bytes()).unwrap();
        archive
            .into_inner()
            .unwrap()
            .finish()
            .unwrap()
            .flush()
            .unwrap();
    }
}
