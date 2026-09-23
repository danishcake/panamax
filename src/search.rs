use std::{
    fs::File,
    io::{self, BufRead, BufReader, Read},
    path::{Path, PathBuf},
    sync::Mutex,
};

use flate2::read::GzDecoder;
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};
use semver::Version;
use serde::Deserialize;
use tar::Archive;
use thiserror::Error;
use walkdir::WalkDir;

use crate::crates::get_crate_path;

/// Name of the database stored at the root of a mirror.
pub const DATABASE_FILENAME: &str = "search.db";

/// The database scheme version
const DATABASE_SCHEMA_VERSION: i32 = 1;

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
}

#[derive(Debug)]
struct VersionEntry {
    version: Version,
    yanked: bool,
}

#[derive(Debug)]
struct CrateRecord {
    name: String,
    latest_version: Option<String>,
    latest_yanked_version: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Manifest {
    package: Option<ManifestPackage>,
    project: Option<ManifestPackage>,
}

#[derive(Debug, Deserialize)]
struct ManifestPackage {
    description: Option<String>,
}

/// Read-only access to a mirror's prebuilt search database.
pub struct SearchIndex {
    connection: Mutex<Connection>,
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
        // Clamp caller-provided values so an HTTP client cannot request an unbounded page.
        let per_page = per_page.unwrap_or(DEFAULT_PER_PAGE).clamp(1, MAX_PER_PAGE);
        let connection = self.connection.lock().map_err(|_| {
            SearchError::InvalidDatabase("search database lock was poisoned".into())
        })?;

        let query = query.trim();
        if query.is_empty() {
            // TODO explain
            return query_database(&connection, None, query, per_page);
        } else if query.chars().count() < 3 {
            // FTS5's trigram tokenizer cannot index one- or two-character terms.
            let pattern = like_literal_pattern(query);
            query_database(&connection, Some(&pattern), query, per_page)
        } else {
            // Quote the user input so FTS5 syntax cannot be injected through the query string.
            let fts_query = fts_literal_query(query);
            query_database(&connection, Some(&fts_query), query, per_page)
        }
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
        "INSERT INTO crates (name, description, latest_version, latest_yanked_version)
         VALUES (?1, ?2, ?3, ?4)",
    )?;
    let mut search_insert = transaction
        .prepare("INSERT INTO crate_search (crate_id, name, description) VALUES (?1, ?2, ?3)")?;

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
        let entry =
            entry.map_err(|error| io::Error::new(io::ErrorKind::Other, error.to_string()))?;
        if !entry.file_type().is_file() {
            continue;
        }

        // Read name, latest version and description
        let path = entry.into_path();
        let Some(record) = read_index_file(mirror_path, &path)? else {
            continue;
        };

        // Add to the database
        crate_insert.execute(params![
            record.name,
            record.description,
            record.latest_version,
            record.latest_yanked_version,
        ])?;

        // The FTS table stores the row ID so query results can recover full crate metadata.
        // We can't use a proper foreign key constraint here
        let crate_id = transaction.last_insert_rowid();
        search_insert.execute(params![crate_id, record.name, record.description])?;
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
    // The trigram tokenizer provides indexed substring matching for names and descriptions.
    connection.execute_batch(&format!(
        "CREATE TABLE IF NOT EXISTS crates (
            id INTEGER PRIMARY KEY,
            name TEXT NOT NULL UNIQUE COLLATE NOCASE,
            description TEXT,
            latest_version TEXT,
            latest_yanked_version TEXT
        );
        CREATE INDEX IF NOT EXISTS crates_name_idx ON crates(name COLLATE NOCASE);
        CREATE VIRTUAL TABLE IF NOT EXISTS crate_search USING fts5(
            crate_id UNINDEXED,
            name,
            description,
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
fn read_index_file(mirror_path: &Path, path: &Path) -> Result<Option<CrateRecord>, SearchError> {
    let file = BufReader::new(File::open(path)?);
    let mut crate_name = None;
    let mut latest_version = None;
    let mut latest_yanked_version = None;

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
        }
    }

    let Some(crate_name) = crate_name else {
        return Ok(None);
    };
    // Descriptions come from the newest usable local archive, not from the index metadata.
    let description_version = latest_version.as_ref().or(latest_yanked_version.as_ref());
    let description = description_version.and_then(|entry| {
        match read_description(mirror_path, &crate_name, &entry.version) {
            Ok(description) => description,
            Err(error) => {
                log::warn!(
                    "Could not read description for {crate_name} {}: {error}",
                    entry.version
                );
                None
            }
        }
    });

    Ok(Some(CrateRecord {
        name: crate_name,
        latest_version: latest_version.map(|entry| entry.version.to_string()),
        latest_yanked_version: latest_yanked_version.map(|entry| entry.version.to_string()),
        description,
    }))
}

/// Reads the description for a given crate
fn read_description(
    mirror_path: &Path,
    crate_name: &str,
    version: &Version,
) -> Result<Option<String>, SearchError> {
    // Selective mirrors may not contain the archive, which is a valid no-description case.
    let crate_path =
        get_crate_path(mirror_path, crate_name, &version.to_string()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Other,
                format!("invalid crate name: {crate_name}"),
            )
        })?;
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
        let manifest: Manifest = toml_edit::easy::from_str(&contents)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))?;
        // `project` supports older crate manifests used by the original search implementation.
        return Ok(manifest
            .package
            .or(manifest.project)
            .and_then(|package| package.description));
    }

    Ok(None)
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
    /// Crates returned for the requested page.
    pub crates: Vec<SearchResult>,
}

fn query_database(
    connection: &Connection,
    search_query: Option<&str>,
    display_query: &str,
    per_page: usize,
) -> Result<SearchResults, SearchError> {
    // Short terms use LIKE because trigram FTS cannot represent them; longer terms use FTS.
    let (where_clause, bind_query) = match search_query {
        Some(_) if display_query.chars().count() < 3 => (
            "WHERE lower(c.name) LIKE lower(?1) ESCAPE '\\' OR lower(COALESCE(c.description, '')) LIKE lower(?1) ESCAPE '\\'",
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

    let query_sql = format!(
        "SELECT c.name, c.description, COALESCE(c.latest_version, '0.0.0')
         FROM crates c
         {where_clause}
         ORDER BY CASE WHEN lower(c.name) = lower(?2) THEN 0
                       WHEN lower(c.name) LIKE lower(?3) THEN 1
                       ELSE 2 END,
                  c.name COLLATE NOCASE
         LIMIT ?4"
    );
    let name_pattern = like_literal_pattern(display_query);
    // The SQL text contains only fixed clauses; every user value is bound as a parameter.
    let mut statement = connection.prepare(&query_sql)?;
    let mut rows = match bind_query {
        Some(query) => statement.query(params![query, display_query, name_pattern, per_page])?,
        None => statement.query(params!["", display_query, name_pattern, per_page])?,
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
        total: usize::try_from(total).unwrap_or(usize::MAX),
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
        assert_eq!(results.crates.len(), 1);
        assert_eq!(results.crates[0].name, "foo");
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
        // Construct only the manifest needed to verify description extraction.
        let crate_path = get_crate_path(mirror_path, name, version).unwrap();
        let file = File::create(crate_path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut archive = Builder::new(encoder);
        let manifest = format!(
            "[package]\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"{description}\"\n"
        );
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
