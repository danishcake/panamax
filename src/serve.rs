use std::{collections::HashMap, io, net::SocketAddr, path::PathBuf, process::Stdio, sync::Arc};

use askama::Template;
use bytes::BytesMut;
use futures_util::stream::TryStreamExt;
use include_dir::{include_dir, Dir};
use percent_encoding::percent_decode_str;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    fs::File,
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    process::{ChildStdout, Command},
};
use tokio_stream::StreamExt;
use tokio_util::codec::{BytesCodec, FramedRead};
use warp::{
    host::Authority,
    http,
    hyper::{body::Sender, Body, Response},
    path::Tail,
    reject::Reject,
    Filter, Rejection, Stream,
};

use crate::crates::get_crate_path;
use crate::search::{CrateDetails, SearchIndex, SearchResult};

pub struct TlsConfig {
    pub cert_path: PathBuf,
    pub key_path: PathBuf,
}

#[derive(PartialEq, Eq, PartialOrd, Ord)]
pub struct Platform {
    is_exe: bool,
    platform_triple: String,
}

#[derive(Template)]
#[template(path = "index.html")]
struct IndexTemplate {
    platforms: Vec<Platform>,
    host: String,
}

#[derive(Template)]
#[template(path = "search.html")]
struct SearchTemplate {
    query: String,
    results: Vec<SearchResult>,
    total: usize,
    page: usize,
    total_pages: usize,
    search_unavailable: bool,
}

#[derive(Template)]
#[template(path = "crate.html")]
struct CrateTemplate {
    details: CrateDetails,
    readme_html: Option<std::sync::Arc<str>>,
}

#[derive(Template)]
#[template(path = "crate_source.html")]
struct CrateSourceTemplate {
    crate_name: String,
    version: String,
    crate_url: String,
    parent_url: Option<String>,
    entries: Vec<CrateSourceEntry>,
}

struct CrateSourceEntry {
    name: String,
    url: String,
    is_directory: bool,
}

const STATIC_DIR: Dir = include_dir!("static");

#[derive(Error, Debug)]
pub enum ServeError {
    #[error("IO error: {0}")]
    Io(#[from] io::Error),
    #[error("Hyper error: {0}")]
    Hyper(#[from] warp::hyper::Error),
    #[error("Warp HTTP error: {0}")]
    Warp(#[from] warp::http::Error),
    #[error("{0}")]
    Other(String),
}

impl Reject for ServeError {}

#[derive(Deserialize)]
struct CratesSearch {
    q: String,
    per_page: Option<usize>,
}

async fn search(
    p: &CratesSearch,
    index: Option<&SearchIndex>,
) -> Result<http::Response<String>, Rejection> {
    // Search is unavailable until the explicit offline build command has created the database.
    let Some(index) = index else {
        return Err(warp::reject::not_found());
    };

    // Search the database
    let results = index
        .search(&p.q, p.per_page)
        .map_err(|_| warp::reject::not_found())?;

    // Convert to Cargo's registry search response structure
    let crates = results
        .crates
        .into_iter()
        .map(|crate_| Crate {
            name: crate_.name,
            description: crate_.description,
            max_version: crate_.max_version,
        })
        .collect::<Vec<_>>();
    let meta = TotalCrates {
        total: results.total,
    };
    let body =
        serde_json::to_string(&Crates { crates, meta }).map_err(|_| warp::reject::not_found())?;
    Response::builder()
        .header(http::header::CONTENT_TYPE, "application/json")
        .body(body)
        .map_err(|_| warp::reject::not_found())
}

/// Show details about an in-browser search
async fn browser_search(
    params: BrowserSearch,
    index: Option<Arc<SearchIndex>>,
) -> Result<SearchTemplate, Rejection> {
    // Extract the query string and page
    let query = params.q.unwrap_or_default().trim().to_string();
    let requested_page = params.page.unwrap_or(1);

    // Crates data is unavailable until the explicit offline build command has created the database.
    // If this hasn't been done we'll just render a basic empty result
    let Some(index) = index else {
        return Ok(SearchTemplate {
            query,
            results: Vec::new(),
            total: 0,
            page: 1,
            total_pages: 1,
            search_unavailable: true,
        });
    };

    // If the query hasn't been specified we'll just show the search box
    if query.is_empty() {
        return Ok(SearchTemplate {
            query,
            results: Vec::new(),
            total: 0,
            page: 1,
            total_pages: 1,
            search_unavailable: false,
        });
    }

    // Search the database
    let results = index
        .search_page(&query, Some(BROWSER_SEARCH_PER_PAGE), requested_page)
        .map_err(|_| warp::reject::not_found())?;
    Ok(SearchTemplate {
        query,
        results: results.crates,
        total: results.total,
        page: results.page,
        total_pages: results.total_pages,
        search_unavailable: false,
    })
}

/// Shows details about a single crate
async fn crate_details(
    name: String,
    index: Option<Arc<SearchIndex>>,
    mirror_path: PathBuf,
) -> Result<CrateTemplate, Rejection> {
    // Crate details are unavailable until the explicit offline build command has created the database.
    let Some(index) = index else {
        return Err(warp::reject::not_found());
    };

    // Search the database
    let details = index
        .crate_details(&name)
        .map_err(|_| warp::reject::not_found())?
        .ok_or_else(warp::reject::not_found)?;

    // Render the crate README to a snippet of HTML
    let readme_html = if let Some(version) = details.latest_available_version.clone() {
        let crate_name = details.name.clone();
        let log_crate_name = crate_name.clone();
        let log_version = version.clone();
        match tokio::task::spawn_blocking(move || {
            crate::readme::render_crate_readme(&mirror_path, &crate_name, &version)
        })
        .await
        {
            Ok(Ok(readme)) => readme,
            Ok(Err(error)) => {
                log::warn!("Could not load README for {log_crate_name} {log_version}: {error}");
                None
            }
            Err(error) => {
                log::warn!(
                    "README rendering task failed for {log_crate_name} {log_version}: {error}"
                );
                None
            }
        }
    } else {
        None
    };
    Ok(CrateTemplate {
        details,
        readme_html,
    })
}

#[derive(Serialize)]
struct Crate {
    name: String,
    description: Option<String>,
    max_version: String,
}

#[derive(Serialize)]
struct TotalCrates {
    total: usize,
}

#[derive(Serialize)]
struct Crates {
    crates: Vec<Crate>,
    meta: TotalCrates,
}

#[derive(Deserialize)]
struct BrowserSearch {
    q: Option<String>,
    page: Option<usize>,
}

const BROWSER_SEARCH_PER_PAGE: usize = 100;

pub async fn serve(path: PathBuf, socket_addr: SocketAddr, tls_paths: Option<TlsConfig>) {
    let index_path = path.clone();
    let is_tls = tls_paths.is_some();
    // Open the database once so requests do not parse JSON or scan mirror files.
    let search_index = match SearchIndex::open(&path) {
        Ok(index) => Some(Arc::new(index)),
        Err(error) => {
            eprintln!(
                "Search index unavailable; run `panamax create-search-index {}`: {error}",
                path.display()
            );
            None
        }
    };

    // Handle the homepage
    let index = warp::path::end().and(warp::host::optional()).and_then(
        move |authority: Option<Authority>| {
            let mirror_path = index_path.clone();
            let protocol = if is_tls { "https://" } else { "http://" };
            async move {
                get_rustup_platforms(mirror_path)
                    .await
                    .map(|platforms| IndexTemplate {
                        platforms,
                        host: authority
                            .map(|a| format!("{}{}", protocol, a.as_str()))
                            .unwrap_or_else(|| "http://panamax.internal".to_string()),
                    })
                    .map_err(|_| {
                        warp::reject::custom(ServeError::Other(
                            "Could not retrieve rustup platforms.".to_string(),
                        ))
                    })
            }
        },
    );

    // Handle the browser search page and crate detail pages separately from Cargo's JSON API.
    let search_page_index = search_index.clone();
    let browser_search_page = warp::path("search.html")
        .and(warp::query::<BrowserSearch>())
        .and(warp::any().map(move || search_page_index.clone()))
        .and_then(browser_search);

    let crate_page_index = search_index.clone();
    let crate_page_mirror_path = path.clone();
    let browser_crate_page = warp::path!("crate" / String)
        .and(warp::any().map(move || crate_page_index.clone()))
        .and(warp::any().map(move || crate_page_mirror_path.clone()))
        .and_then(crate_details);

    // Serve paths referenced by crate READMEs from the corresponding local archive.
    let crate_source_mirror_path = path.clone();
    let crate_source_file = warp::path("crate")
        .and(warp::path::param::<String>())
        .and(warp::path::param::<String>())
        .and(warp::path("source"))
        .and(warp::path::tail())
        .and(warp::get())
        .and_then(move |name: String, version: String, file_path: Tail| {
            let mirror_path = crate_source_mirror_path.clone();
            async move { crate_source_file_response(mirror_path, name, version, file_path).await }
        });

    // Handle `cargo search` queries ("/crates?q={}&per_page={}")
    let search_index_for_route = search_index.clone();
    let crates_search = warp::path::path("crates")
        .and(warp::query::<CratesSearch>())
        .and_then(move |p: CratesSearch| {
            let search_index = search_index_for_route.clone();
            async move { search(&p, search_index.as_deref()).await }
        });

    // Handle all files baked into the binary with include_dir, at /static
    let static_dir =
        warp::path::path("static")
            .and(warp::path::tail())
            .and_then(|path: Tail| async move {
                STATIC_DIR
                    .get_file(path.as_str())
                    .ok_or_else(warp::reject::not_found)
                    .map(|f| f.contents().to_vec())
            });

    let dist_dir = warp::path::path("dist").and(warp::fs::dir(path.join("dist")));
    let rustup_dir = warp::path::path("rustup").and(warp::fs::dir(path.join("rustup")));

    // Handle crates requests in the format of "/crates/ripgrep/0.1.0/download"
    // This format is the default for cargo, and will be used if an external process rewrites config.json in crates.io-index
    let crates_mirror_path = path.clone();
    let crates_dir_native_format = warp::path!("crates" / String / String / "download").and_then(
        move |name: String, version: String| {
            let mirror_path = crates_mirror_path.clone();
            async move { get_crate_file(mirror_path, &name, &version).await }
        },
    );

    // Handle crates requests in the format of either :
    // - "/crates/1/u/0.2.0/u-0.2.0.crate"
    // - "/crates/2/bm/0.11.0/bm-0.11.0.crate"
    // - "/crates/3/c/cde/0.1.1/cde-0.1.1.crate"
    // - "/crates/se/rd/serde/1.0.130/serde-1.0.130.crate"
    // This format is used by Panamax, and/or is used if config.json contains "/crates/{prefix}/{crate}/{version}/{crate}-{version}.crate"
    let crates_mirror_path_2 = path.clone();
    let crates_dir_condensed_format_1 = warp::path!("crates" / "1" / String / String / String)
        .map(|name: String, version: String, crate_file: String| (name, version, crate_file))
        .untuple_one();
    let crates_dir_condensed_format_2 = warp::path!("crates" / "2" / String / String / String)
        .map(|name: String, version: String, crate_file: String| (name, version, crate_file))
        .untuple_one();
    let crates_dir_condensed_format_3 =
        warp::path!("crates" / "3" / String / String / String / String)
            .map(
                |_: String, name: String, version: String, crate_file: String| {
                    (name, version, crate_file)
                },
            )
            .untuple_one();
    let crates_dir_condensed_format_full =
        warp::path!("crates" / String / String / String / String / String)
            .map(
                |_: String, _: String, name: String, version: String, crate_file: String| {
                    (name, version, crate_file)
                },
            )
            .untuple_one();

    let crates_dir_condensed_format = crates_dir_condensed_format_1
        .or(crates_dir_condensed_format_2)
        .unify()
        .or(crates_dir_condensed_format_3)
        .unify()
        .or(crates_dir_condensed_format_full)
        .unify()
        .and_then(move |name: String, version: String, crate_file: String| {
            let mirror_path = crates_mirror_path_2.clone();
            async move {
                if !crate_file.ends_with(".crate") || !crate_file.starts_with(&name) {
                    return Err(warp::reject::not_found());
                }
                get_crate_file(mirror_path, &name, &version).await
            }
        });

    // Handle git client requests to /git/crates.io-index
    let path_for_git = path.clone();
    let git = warp::path("git")
        .and(warp::path("crates.io-index"))
        .and(warp::path::tail())
        .and(warp::method())
        .and(warp::header::optional::<String>("Content-Type"))
        .and(warp::addr::remote())
        .and(warp::body::stream())
        .and(warp::query::raw().or_else(|_| async { Ok::<(String,), Rejection>((String::new(),)) }))
        .and_then(
            move |path_tail, method, content_type, remote, body, query| {
                let mirror_path = path_for_git.clone();
                async move {
                    handle_git(
                        mirror_path,
                        path_tail,
                        method,
                        content_type,
                        remote,
                        body,
                        query,
                    )
                    .await
                }
            },
        );

    // Handle sparse index requests at /index/
    let sparse_index = warp::path("index").and(warp::fs::dir(path.join("crates.io-index")));

    let routes = index
        .or(browser_search_page)
        .or(crate_source_file)
        .or(browser_crate_page)
        .or(static_dir)
        .or(dist_dir)
        .or(rustup_dir)
        .or(crates_search)
        .or(crates_dir_native_format)
        .or(crates_dir_condensed_format)
        .or(sparse_index)
        .or(git);

    match tls_paths {
        Some(TlsConfig {
            cert_path,
            key_path,
        }) => {
            println!("Running TLS on {socket_addr}");
            warp::serve(routes)
                .tls()
                .cert_path(cert_path)
                .key_path(key_path)
                .run(socket_addr)
                .await;
        }
        None => {
            println!("Running HTTP on {socket_addr}");
            warp::serve(routes).run(socket_addr).await;
        }
    }
}

async fn crate_source_file_response(
    mirror_path: PathBuf,
    crate_name: String,
    version: String,
    file_path: Tail,
) -> Result<http::Response<Vec<u8>>, Rejection> {
    let decoded_path = percent_decode_str(file_path.as_str())
        .decode_utf8()
        .map_err(|_| warp::reject::not_found())?;
    let relative_path = PathBuf::from(decoded_path.as_ref());
    let contents = if relative_path.as_os_str().is_empty() {
        None
    } else {
        let read_path = relative_path.clone();
        let file_mirror_path = mirror_path.clone();
        let file_crate_name = crate_name.clone();
        let file_version = version.clone();
        Some(
            tokio::task::spawn_blocking(move || {
                crate::readme::read_crate_file(
                    &file_mirror_path,
                    &file_crate_name,
                    &file_version,
                    &read_path,
                )
            })
            .await
            .map_err(|_| warp::reject::not_found())?
            .map_err(|_| warp::reject::not_found())?,
        )
    };

    if let Some(Some(contents)) = contents {
        let content_type = crate_file_content_type(&relative_path);
        return source_response(contents, &content_type);
    }

    let listing_mirror_path = mirror_path.clone();
    let listing_crate_name = crate_name.clone();
    let listing_version = version.clone();
    let listing_path = relative_path.clone();
    let entries = tokio::task::spawn_blocking(move || {
        crate::readme::list_crate_directory(
            &listing_mirror_path,
            &listing_crate_name,
            &listing_version,
            &listing_path,
        )
    })
    .await
    .map_err(|_| warp::reject::not_found())?
    .map_err(|_| warp::reject::not_found())?
    .ok_or_else(warp::reject::not_found)?;

    let listing = render_crate_directory_listing(&crate_name, &version, &relative_path, &entries)?;
    source_response(listing.into_bytes(), "text/html; charset=utf-8")
}

fn source_response(
    body: Vec<u8>,
    content_type: &str,
) -> Result<http::Response<Vec<u8>>, Rejection> {
    let mut response = http::Response::builder()
        .header(http::header::CONTENT_TYPE, content_type)
        .header("X-Content-Type-Options", "nosniff");
    if is_document_content_type(content_type) {
        response = response.header(
            "Content-Security-Policy",
            "sandbox allow-downloads allow-top-navigation-by-user-activation",
        );
    }
    response
        .body(body)
        .map_err(|error| warp::reject::custom(ServeError::from(error)))
}

fn crate_file_content_type(path: &std::path::Path) -> String {
    let content_type = mime_guess::from_path(path)
        .first_or_octet_stream()
        .essence_str()
        .to_owned();
    if content_type == "application/x-sh" {
        "text/plain; charset=utf-8".to_owned()
    } else {
        content_type
    }
}

fn is_document_content_type(content_type: &str) -> bool {
    matches!(
        content_type.split(';').next().map(str::trim),
        Some("text/html" | "application/xhtml+xml" | "image/svg+xml")
    )
}

fn render_crate_directory_listing(
    crate_name: &str,
    version: &str,
    path: &std::path::Path,
    entries: &[crate::readme::CrateDirectoryEntry],
) -> Result<String, Rejection> {
    let entries = entries
        .iter()
        .map(|entry| {
            let entry_path = path.join(&entry.name);
            let encoded_path = encode_archive_path(&entry_path);
            CrateSourceEntry {
                name: entry.name.clone(),
                url: format!(
                    "/crate/{crate_name}/{version}/source/{encoded_path}{}",
                    if entry.is_directory { "/" } else { "" }
                ),
                is_directory: entry.is_directory,
            }
        })
        .collect();
    CrateSourceTemplate {
        crate_name: crate_name.to_owned(),
        version: version.to_owned(),
        crate_url: format!("/crate/{crate_name}"),
        parent_url: (!path.as_os_str().is_empty()).then(|| {
            let parent_path = path.parent().unwrap_or_else(|| std::path::Path::new(""));
            let encoded_parent = encode_archive_path(parent_path);
            if encoded_parent.is_empty() {
                format!("/crate/{crate_name}/{version}/source/")
            } else {
                format!("/crate/{crate_name}/{version}/source/{encoded_parent}/")
            }
        }),
        entries,
    }
    .render()
    .map_err(|error| warp::reject::custom(ServeError::Other(error.to_string())))
}

fn encode_archive_path(path: &std::path::Path) -> String {
    path.components()
        .filter_map(|component| match component {
            std::path::Component::Normal(segment) => segment.to_str(),
            _ => None,
        })
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

fn encode_path_segment(segment: &str) -> String {
    let mut encoded = String::new();
    for byte in segment.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~".contains(&byte) {
            encoded.push(byte as char);
        } else {
            encoded.push_str(&format!("%{byte:02X}"));
        }
    }
    encoded
}

/// Get all rustup platforms available on the mirror.
async fn get_rustup_platforms(path: PathBuf) -> io::Result<Vec<Platform>> {
    let rustup_path = path.join("rustup/dist");

    let mut output = vec![];

    // Look at the rustup/dist directory for all rustup-init and rustup-init.exe files.
    // Also return if the rustup-init file is a .exe or not.
    if let Ok(mut rd) = tokio::fs::read_dir(rustup_path).await {
        while let Some(entry) = rd.next_entry().await? {
            if entry.metadata().await?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    let platform_triple = name.to_string();
                    if entry.path().join("rustup-init").exists() {
                        output.push(Platform {
                            is_exe: false,
                            platform_triple,
                        });
                    } else if entry.path().join("rustup-init.exe").exists() {
                        output.push(Platform {
                            is_exe: true,
                            platform_triple,
                        });
                    }
                }
            }
        }
    }

    // Sort by name, keeping non-exe versions at the top.
    output.sort();

    Ok(output)
}

/// Return a crate file as an HTTP response.
async fn get_crate_file(
    mirror_path: PathBuf,
    name: &str,
    version: &str,
) -> Result<Response<Body>, Rejection> {
    let full_path =
        get_crate_path(&mirror_path, name, version).ok_or_else(warp::reject::not_found)?;

    let file = File::open(full_path)
        .await
        .map_err(|_| warp::reject::not_found())?;
    let meta = file
        .metadata()
        .await
        .map_err(|_| warp::reject::not_found())?;
    let stream = FramedRead::new(file, BytesCodec::new()).map_ok(BytesMut::freeze);

    let body = Body::wrap_stream(stream);

    let mut resp = Response::new(body);
    resp.headers_mut()
        .insert(http::header::CONTENT_LENGTH, meta.len().into());
    let filename = format!("{name}-{version}.crate")
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    let content_disposition =
        http::HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .map_err(|_| warp::reject::not_found())?;
    resp.headers_mut()
        .insert(http::header::CONTENT_DISPOSITION, content_disposition);

    Ok(resp)
}

/// Handle a request from a git client.
async fn handle_git<S, B>(
    mirror_path: PathBuf,
    path_tail: Tail,
    method: http::Method,
    content_type: Option<String>,
    remote: Option<SocketAddr>,
    mut body: S,
    query: String,
) -> Result<Response<Body>, Rejection>
where
    S: Stream<Item = Result<B, warp::Error>> + Send + Unpin + 'static,
    B: bytes::Buf + Sized,
{
    let remote = remote
        .map(|r| r.ip().to_string())
        .unwrap_or_else(|| "127.0.0.1".to_string());

    // Run "git http-backend"
    let mut cmd = Command::new("git");
    cmd.arg("http-backend");

    // Clear environment variables, and set needed variables
    // See: https://git-scm.com/docs/git-http-backend
    cmd.env_clear();
    cmd.env("GIT_PROJECT_ROOT", mirror_path);
    cmd.env(
        "PATH_INFO",
        format!("/crates.io-index/{}", path_tail.as_str()),
    );
    cmd.env("REQUEST_METHOD", method.as_str());
    cmd.env("QUERY_STRING", query);
    cmd.env("REMOTE_USER", "");
    cmd.env("REMOTE_ADDR", remote);
    if let Some(content_type) = content_type {
        cmd.env("CONTENT_TYPE", content_type);
    }
    cmd.env("GIT_HTTP_EXPORT_ALL", "true");
    cmd.stderr(Stdio::inherit());
    cmd.stdout(Stdio::piped());
    cmd.stdin(Stdio::piped());

    let p = cmd.spawn().map_err(ServeError::from)?;

    // Handle sending git client body to http-backend, if any
    let mut git_input = p.stdin.expect("Process should always have stdin");
    while let Some(Ok(mut buf)) = body.next().await {
        git_input
            .write_all_buf(&mut buf)
            .await
            .map_err(ServeError::from)?;
    }

    // Collect headers from git CGI output
    let mut git_output = BufReader::new(p.stdout.expect("Process should always have stdout"));
    let mut headers = HashMap::new();
    loop {
        let mut line = String::new();
        git_output
            .read_line(&mut line)
            .await
            .map_err(ServeError::from)?;

        let line = line.trim_end();
        if line.is_empty() {
            break;
        }

        if let Some((key, value)) = line.split_once(": ") {
            headers.insert(key.to_string(), value.to_string());
        }
    }

    // Add headers to response (except for Status, which is the "200 OK" line)
    let mut resp = Response::builder();
    for (key, val) in headers {
        if key == "Status" {
            resp = resp.status(&val.as_bytes()[..3]);
        } else {
            resp = resp.header(&key, val);
        }
    }

    // Create channel, so data can be streamed without being fully loaded
    // into memory. Requires a separate future to be spawned.
    let (sender, body) = Body::channel();
    tokio::spawn(send_git(sender, git_output));

    let resp = resp.body(body).map_err(ServeError::from)?;
    Ok(resp)
}

/// Send data from git CGI process to hyper Sender, until there is no more
/// data left.
async fn send_git(
    mut sender: Sender,
    mut git_output: BufReader<ChildStdout>,
) -> Result<(), ServeError> {
    loop {
        let mut bytes_out = BytesMut::new();
        git_output.read_buf(&mut bytes_out).await?;
        if bytes_out.is_empty() {
            return Ok(());
        }
        sender.send_data(bytes_out.freeze()).await?;
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use askama::Template;

    #[test]
    fn search_template_escapes_query_and_links_results() {
        let template = SearchTemplate {
            query: "<serde>".to_string(),
            results: vec![SearchResult {
                name: "serde".to_string(),
                description: Some("Serialization & deserialization".to_string()),
                max_version: "1.0.0".to_string(),
            }],
            total: 1,
            page: 1,
            total_pages: 3,
            search_unavailable: false,
        };

        let rendered = template.render().unwrap();
        assert!(rendered.contains("value=\"&lt;serde&gt;\""));
        assert!(rendered.contains("href=\"/crate/serde\""));
        assert!(rendered.contains("Serialization &amp; deserialization"));
        assert_eq!(rendered.matches("Page 1 of 3").count(), 2);
        assert!(rendered.contains("name=\"q\" value=\"&lt;serde&gt;\""));
        assert_eq!(rendered.matches("name=\"page\" value=\"2\"").count(), 2);
    }

    #[test]
    fn crate_template_renders_metadata_and_download_link() {
        let template = CrateTemplate {
            details: CrateDetails {
                name: "serde".to_string(),
                max_version: "1.0.0".to_string(),
                latest_yanked_version: Some("0.9.0".to_string()),
                latest_available_version: Some("1.0.0".to_string()),
                available_versions: vec![crate::search::AvailableVersion {
                    version: "1.0.0".to_string(),
                    yanked: false,
                }],
                metadata: crate::search::CrateMetadata {
                    description: Some("Serialization framework".to_string()),
                    authors: vec!["The Serde team".to_string()],
                    license: Some("MIT OR Apache-2.0".to_string()),
                    repository: Some("https://github.com/serde-rs/serde".to_string()),
                    homepage: Some("javascript:alert(1)".to_string()),
                    documentation: Some("data:text/html,unsafe".to_string()),
                    keywords: vec!["serialization".to_string()],
                    categories: vec!["encoding".to_string()],
                    dependencies: vec![crate::search::Dependency {
                        name: "serde_derive".to_string(),
                        requirement: "1".to_string(),
                        kind: "dependencies".to_string(),
                    }],
                },
            },
            readme_html: Some(Arc::from("<h2>README content</h2>")),
        };

        let rendered = template.render().unwrap();
        assert!(rendered.contains("<h2>README content</h2>"));
        assert!(rendered.contains("/crates/serde/1.0.0/download"));
        assert!(rendered.contains("MIT OR Apache-2.0"));
        assert!(rendered.contains("href=\"https://github.com/serde-rs/serde\""));
        assert!(rendered.contains("javascript:alert(1)"));
        assert!(rendered.contains("data:text/html,unsafe"));
        assert!(!rendered.contains("href=\"javascript:alert(1)\""));
        assert!(!rendered.contains("href=\"data:text/html,unsafe\""));
        assert!(rendered.contains("href=\"/crate/serde_derive\""));
        assert!(rendered.contains("serde_derive"));
        assert!(rendered.contains("href=\"/crate/serde/1.0.0/source/\">Browse contents</a>"));
        assert!(rendered.find("README</h2>").unwrap() < rendered.find("Versions</h2>").unwrap());
    }

    #[test]
    fn crate_source_listing_escapes_names_and_encodes_links() {
        let rendered = render_crate_directory_listing(
            "foo",
            "1.0.0",
            std::path::Path::new("examples"),
            &[crate::readme::CrateDirectoryEntry {
                name: "demo <one>.rs".to_owned(),
                is_directory: false,
            }],
        )
        .unwrap();

        assert!(rendered.contains("class=\"browser-content crate-page-content\""));
        assert!(rendered.contains("class=\"detail-section crate-source-listing\""));
        assert!(rendered.contains(
            "href=\"/crate/foo/1.0.0/source/examples/demo%20%3Cone%3E.rs\">demo &lt;one&gt;.rs</a>"
        ));
        assert!(rendered.contains("href=\"/crate/foo/1.0.0/source/\">..</a>"));
        assert!(!rendered.contains("demo <one>.rs</a>"));

        let root_listing =
            render_crate_directory_listing("foo", "1.0.0", std::path::Path::new(""), &[]).unwrap();
        assert!(!root_listing.contains(">..</a>"));
    }

    #[test]
    fn shell_scripts_are_renderable_and_non_documents_are_not_sandboxed() {
        assert_eq!(
            crate_file_content_type(std::path::Path::new("install-build-tools.sh")),
            "text/plain; charset=utf-8"
        );

        let shell_response = source_response(
            b"#!/bin/sh\necho hello".to_vec(),
            "text/plain; charset=utf-8",
        )
        .unwrap();
        assert_eq!(
            shell_response
                .headers()
                .get(http::header::CONTENT_TYPE)
                .unwrap(),
            "text/plain; charset=utf-8"
        );
        assert!(shell_response
            .headers()
            .get("Content-Security-Policy")
            .is_none());
        assert_eq!(shell_response.body(), b"#!/bin/sh\necho hello");

        let html_response =
            source_response(b"<p>contents</p>".to_vec(), "text/html; charset=utf-8").unwrap();
        assert_eq!(
            html_response
                .headers()
                .get("Content-Security-Policy")
                .unwrap(),
            "sandbox allow-downloads allow-top-navigation-by-user-activation"
        );
    }

    #[tokio::test]
    async fn crate_download_sets_archive_filename() {
        let temporary_directory = tempfile::tempdir().unwrap();
        let crate_path = get_crate_path(temporary_directory.path(), "foo", "1.2.3").unwrap();
        fs::create_dir_all(crate_path.parent().unwrap()).unwrap();
        fs::write(&crate_path, b"crate contents").unwrap();

        let response = get_crate_file(temporary_directory.path().to_path_buf(), "foo", "1.2.3")
            .await
            .unwrap();
        assert_eq!(
            response
                .headers()
                .get(http::header::CONTENT_DISPOSITION)
                .unwrap(),
            "attachment; filename=\"foo-1.2.3.crate\""
        );
    }
}
