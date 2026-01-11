use std::num::{NonZeroU8, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::env;
use std::fs;
use anyhow::Result;
use tracing::{info, error, warn};
use url::Url;
use reqwest::{Client, header, Error as ReqwestError};
use http_downloader::{HttpDownloaderBuilder, speed_tracker::DownloadSpeedTrackerExtension, status_tracker::DownloadStatusTrackerExtension, DownloadingEndCause};
use http_downloader::speed_limiter::DownloadSpeedLimiterExtension;
use regex::Regex;
use percent_encoding::percent_decode;
use tokio::sync::Semaphore;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use tokio::time::timeout;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tracing_subscriber::fmt;
use tracing_appender::rolling;

async fn download_file_task(
    url_to_download: String,
    save_dir: PathBuf,
    client: Arc<Client>,
    pb: Arc<ProgressBar>,
    skip_head: bool,
) -> Result<()> {
    info!("Starting download for: {}", url_to_download);

    // Validate URL
    if url_to_download.is_empty() || (!url_to_download.starts_with("http://") && !url_to_download.starts_with("https://")) {
        error!("Invalid URL '{}'. Please provide a valid http/https URL.", url_to_download);
        pb.finish_with_message(format!("Download of {} failed: Invalid URL", url_to_download));
        return Err(anyhow::anyhow!("Invalid URL: {}. Must be http/https.", url_to_download));
    }

    // Resolve final URL (handles redirects) with retry
    const MAX_GET_RETRIES: u8 = 3;
    let retry_count = AtomicU8::new(0);
    let final_url_str = loop {
        let attempt = retry_count.fetch_add(1, Ordering::SeqCst) + 1;
        let result = async {
            if url_to_download.ends_with(".data") {
                return Ok(url_to_download.clone());
            }

            let mut headers = header::HeaderMap::new();
            headers.insert(header::ACCEPT, "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7".parse()?);
            headers.insert(header::ACCEPT_ENCODING, "gzip, deflate, br, zstd".parse()?);
            headers.insert(header::ACCEPT_LANGUAGE, "en-US,en;q=0.9".parse()?);
            headers.insert(header::COOKIE, "vamhubconsent=yes".parse()?);
            headers.insert(header::DNT, "1".parse()?);
            headers.insert(header::HeaderName::from_static("sec-ch-ua"), "\"Not)A;Brand\";v=\"99\", \"Microsoft Edge\";v=\"127\", \"Chromium\";v=\"127\"".parse()?);
            headers.insert(header::HeaderName::from_static("sec-ch-ua-mobile"), "?0".parse()?);
            headers.insert(header::HeaderName::from_static("sec-ch-ua-platform"), "\"Windows\"".parse()?);
            headers.insert(header::HeaderName::from_static("sec-fetch-dest"), "document".parse()?);
            headers.insert(header::HeaderName::from_static("sec-fetch-mode"), "navigate".parse()?);
            headers.insert(header::HeaderName::from_static("sec-fetch-site"), "none".parse()?);
            headers.insert(header::HeaderName::from_static("sec-fetch-user"), "?1".parse()?);
            headers.insert(header::UPGRADE_INSECURE_REQUESTS, "1".parse()?);
            headers.insert(header::USER_AGENT, "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36 Edg/127.0.0.0".parse()?);

            let response = client.get(&url_to_download).headers(headers).send().await?;
            info!("GET request for {} returned status: {}", url_to_download, response.status());
            if response.status() == reqwest::StatusCode::SEE_OTHER {
                if let Some(location) = response.headers().get(header::LOCATION) {
                    let location_str = location.to_str()?.to_string();
                    info!("Redirected to: {}", location_str);
                    Ok(location_str)
                } else {
                    warn!("No redirect location found for {}, using original.", url_to_download);
                    Ok(url_to_download.clone())
                }
            } else {
                let status = response.status();
                if !status.is_success() {
                    let body = response.text().await.unwrap_or_default();
                    error!("GET request for {} failed with status {}: {}", url_to_download, status, body);
                    return Err(anyhow::anyhow!("GET request for {} failed with status {}: {}", url_to_download, status, body));
                }
                Ok(url_to_download.clone())
            }
        }.await;

        match result {
            Ok(url) => break url,
            Err(e) => {
                if attempt <= MAX_GET_RETRIES && is_retryable_error(&e) {
                    warn!("GET request for {} failed: {}. Retrying (attempt {}/{})", url_to_download, e, attempt + 1, MAX_GET_RETRIES + 1);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                } else {
                    error!("GET request for {} failed after {} attempts: {}", url_to_download, attempt, e);
                    pb.finish_with_message(format!("Download of {} failed: Network error: {}", url_to_download, e));
                    return Err(anyhow::anyhow!("Failed to send GET request for {}: {}", url_to_download, e));
                }
            }
        }
    };

    let filename = {
        let mut extracted_filename = if skip_head {
            // Skip HEAD request, extract filename from URL
            info!("Skipping HEAD request for {}, extracting filename from URL", final_url_str);
            let url_path = Url::parse(&final_url_str).ok()
                .and_then(|u| u.path_segments().and_then(|s| s.last().map(|s| s.to_string())));
            url_path.unwrap_or_else(|| "default_filename".to_string())
        } else {
            // HEAD request with retry logic
            const MAX_HEAD_RETRIES: u8 = 3;
            let head_retry_count = AtomicU8::new(0);
            let head_result: Result<Option<String>> = loop {
                let attempt = head_retry_count.fetch_add(1, Ordering::SeqCst) + 1;
                let result = client.head(&final_url_str).send().await;

                match result {
                    Ok(resp) => {
                        info!("HEAD request for {} returned status: {}", final_url_str, resp.status());
                        let content_disposition = resp.headers().get(header::CONTENT_DISPOSITION).cloned();
                        break Ok(content_disposition.and_then(|cd_val| {
                            let cd_str = percent_decode(cd_val.as_bytes()).decode_utf8().unwrap_or_else(|_| "".into());
                            let re = Regex::new(r#"filename(?:\*)?=(?:"([^"]+)"|([^;\s]+))(?:;.*)?$"#).ok()?;
                            re.captures(&cd_str).and_then(|captures| {
                                captures.get(1).or(captures.get(2)).map(|m| m.as_str().to_string())
                            })
                        }));
                    }
                    Err(e) => {
                        let is_retryable = e.is_connect() || e.is_timeout() || e.is_request();
                        if attempt <= MAX_HEAD_RETRIES && is_retryable {
                            warn!("HEAD request for {} failed: {}. Retrying (attempt {}/{})", final_url_str, e, attempt + 1, MAX_HEAD_RETRIES + 1);
                            tokio::time::sleep(Duration::from_secs(2)).await;
                            continue;
                        } else {
                            error!("HEAD request for {} failed after {} attempts: {}", final_url_str, attempt, e);
                            pb.finish_with_message(format!("Download of {} failed: HEAD request error: {}", final_url_str, e));
                            break Err(anyhow::anyhow!("Failed to send HEAD request for {}: {}", final_url_str, e));
                        }
                    }
                }
            };

            match head_result {
                Ok(Some(name)) => name,
                Ok(None) => {
                    // No Content-Disposition, extract from URL
                    Path::new(&final_url_str)
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "default_filename".to_string())
                }
                Err(e) => return Err(e),
            }
        };

        extracted_filename = extracted_filename.trim_end_matches(';').to_string();
        let invalid_chars = ['\\', '/', ':', '*', '?', '"', '<', '>', '|'];
        for c in invalid_chars.iter() {
            extracted_filename = extracted_filename.replace(*c, "_");
        }
        if extracted_filename.is_empty() {
            extracted_filename = "default_filename".to_string();
        }
        extracted_filename
    };
    info!("Resolved filename for {}: {}", final_url_str, filename);

    let download_url_obj = match Url::parse(&final_url_str) {
        Ok(url) => url,
        Err(e) => {
            error!("Failed to parse URL {}: {}", final_url_str, e);
            pb.finish_with_message(format!("Download of {} failed: URL parse error: {}", final_url_str, e));
            return Err(anyhow::anyhow!("Failed to parse URL {}: {}", final_url_str, e));
        }
    };

    // Download with retry logic
    const MAX_RETRIES: u8 = 3;
    let retry_count = AtomicU8::new(0);
    let download_result = loop {
        let attempt = retry_count.fetch_add(1, Ordering::SeqCst) + 1;
        let result = timeout(Duration::from_secs(300), async {
            let (mut downloader, (_status_state, _speed_state, _speed_limiter, ..)) =
                HttpDownloaderBuilder::new(download_url_obj.clone(), save_dir.clone())
                    .chunk_size(NonZeroUsize::new(1024 * 1024 * 10).unwrap())
                    .download_connection_count(NonZeroU8::new(4).unwrap())
                    .build((
                        DownloadStatusTrackerExtension { log: true },
                        DownloadSpeedTrackerExtension { log: true },
                        DownloadSpeedLimiterExtension::new(None),
                    ));

            info!("[{}] Prepare download (attempt {})", filename, attempt);
            let download_future = match downloader.prepare_download() {
                Ok(future) => future,
                Err(e) => {
                    error!("[{}] Failed to prepare download: {}", filename, e);
                    return Err(anyhow::anyhow!("Failed to prepare download: {}", e));
                }
            };

            let pb_clone = Arc::clone(&pb);
            let progress_handle = tokio::spawn({
                let mut downloaded_len_receiver = downloader.downloaded_len_receiver().clone();
                let total_size_future = downloader.total_size_future();
                let p_filename = filename.clone();
                async move {
                    let total_len = total_size_future.await;
                    if let Some(total_len) = total_len {
                        if total_len.get() > 0 {
                            pb_clone.set_length(total_len.get());
                        }
                    }
                    pb_clone.set_message(format!("Downloading {}", p_filename));
                    while downloaded_len_receiver.changed().await.is_ok() {
                        let progress = *downloaded_len_receiver.borrow();
                        pb_clone.set_position(progress);
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                    }
                }
            });

            info!("[{}] Start downloading until the end (attempt {})", filename, attempt);
            let dec = match download_future.await {
                Ok(dec) => dec,
                Err(e) => {
                    error!("[{}] Download failed: {}", filename, e);
                    return Err(anyhow::anyhow!("Download failed: {}", e));
                }
            };
            progress_handle.abort();
            Ok::<DownloadingEndCause, anyhow::Error>(dec)
        }).await;

        match result {
            Ok(Ok(dec)) => break Ok(dec),
            Ok(Err(e)) => {
                if attempt <= MAX_RETRIES && is_retryable_error(&e) {
                    warn!("[{}] Download failed: {}. Retrying (attempt {}/{})", filename, e, attempt + 1, MAX_RETRIES + 1);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                } else {
                    error!("[{}] Download failed after {} attempts: {}", filename, attempt, e);
                    pb.finish_with_message(format!("Download of {} failed: {}", filename, e));
                    break Err(e);
                }
            }
            Err(_) => {
                if attempt <= MAX_RETRIES {
                    warn!("[{}] Download timed out. Retrying (attempt {}/{})", filename, attempt + 1, MAX_RETRIES + 1);
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                } else {
                    error!("[{}] Download timed out after {} attempts", filename, attempt);
                    pb.finish_with_message(format!("Download of {} failed: Timed out", filename));
                    break Err(anyhow::anyhow!("Download timed out after {} attempts", attempt));
                }
            }
        }
    };

    let dec = download_result?;
    info!("[{}] Downloading end cause: {:?}", filename, dec);
    pb.finish_with_message(format!("Download of {} complete", filename));

    // Verify and rename file
    let downloaded_file_path = save_dir.join(download_url_obj.path_segments().and_then(|s| s.last()).unwrap_or("unknown_temp_file"));
    let new_file_path = save_dir.join(&filename);

    if downloaded_file_path == new_file_path {
        info!("[{}] File already at target location: {:?}", filename, new_file_path);
    } else if downloaded_file_path.exists() {
        let file_size = match fs::metadata(&downloaded_file_path) {
            Ok(metadata) => metadata.len(),
            Err(e) => {
                error!("[{}] Failed to get metadata for {:?}: {}", filename, downloaded_file_path, e);
                pb.finish_with_message(format!("Download of {} failed: Metadata error: {}", filename, e));
                return Err(anyhow::anyhow!("Failed to get metadata for {}: {}", filename, e));
            }
        };
        if file_size == 0 {
            error!("[{}] Downloaded file is empty: {:?}", filename, downloaded_file_path);
            pb.finish_with_message(format!("Download of {} failed: Empty file", filename));
            return Err(anyhow::anyhow!("Downloaded file {} is empty", filename));
        }

        match fs::rename(&downloaded_file_path, &new_file_path) {
            Ok(_) => info!("[{}] File renamed to: {:?}", filename, new_file_path),
            Err(e) => {
                error!("[{}] File rename failed from {:?} to {:?}: {}", filename, downloaded_file_path, new_file_path, e);
                pb.finish_with_message(format!("Download of {} failed: Rename error: {}", filename, e));
                return Err(anyhow::anyhow!("File rename failed for {}: {}", filename, e));
            }
        }
    } else {
        warn!("[{}] Expected downloaded file not found at {:?}. Checking target location {:?}", filename, downloaded_file_path, new_file_path);
        if !new_file_path.exists() {
            error!("[{}] Final file {:?} not found. Download may have failed silently.", filename, new_file_path);
            pb.finish_with_message(format!("Download of {} failed: File not found", filename));
            return Err(anyhow::anyhow!("Final file {} not found after download.", filename));
        } else {
            let file_size = match fs::metadata(&new_file_path) {
                Ok(metadata) => metadata.len(),
                Err(e) => {
                    error!("[{}] Failed to get metadata for {:?}: {}", filename, new_file_path, e);
                    pb.finish_with_message(format!("Download of {} failed: Metadata error: {}", filename, e));
                    return Err(anyhow::anyhow!("Failed to get metadata for {}: {}", filename, e));
                }
            };
            if file_size == 0 {
                error!("[{}] Downloaded file at target location is empty: {:?}", filename, new_file_path);
                pb.finish_with_message(format!("Download of {} failed: Empty file", filename));
                return Err(anyhow::anyhow!("Downloaded file {} is empty", filename));
            }
            info!("[{}] File found at target location: {:?}", filename, new_file_path);
        }
    }

    Ok(())
}

// Helper function to determine if an error is retryable
fn is_retryable_error(e: &anyhow::Error) -> bool {
    let error_str = e.to_string().to_lowercase();
    error_str.contains("error sending request") ||
        error_str.contains("connection") ||
        error_str.contains("timeout") ||
        error_str.contains("dns") ||
        if let Some(reqwest_err) = e.downcast_ref::<ReqwestError>() {
            reqwest_err.is_connect() || reqwest_err.is_timeout() || reqwest_err.is_request()
        } else {
            false
        }
}

async fn resolve_url_input(url_input_str: &str, client: &Arc<Client>) -> Result<Vec<String>> {
    let path = PathBuf::from(url_input_str);
    // Check if it's a file first
    if fs::metadata(&path).is_ok() && path.is_file() {
        info!("Input is a local file: {}", url_input_str);
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(e) => {
                error!("Failed to read file {}: {}", url_input_str, e);
                return Err(anyhow::anyhow!("Failed to read file {}: {}", url_input_str, e));
            }
        };
        let urls: Vec<String> = content.lines().map(str::trim).filter(|s| !s.is_empty() && (s.starts_with("http://") || s.starts_with("https://"))).map(String::from).collect();
        if urls.is_empty() {
            warn!("File {} is empty or contains no valid HTTP/HTTPS URLs.", url_input_str);
        }
        return Ok(urls);
    }

    // If not a file, check if it's an HTTP/S URL (could be single or JSON list)
    if url_input_str.starts_with("http://") || url_input_str.starts_with("https://") {
        info!("Input is a URL: {}. Checking if it's a JSON list.", url_input_str);
        match client.get(url_input_str).send().await {
            Ok(response) => {
                let status = response.status();
                info!("GET request for JSON list {} returned status: {}", url_input_str, status);
                if response.status().is_success() {
                    match response.json::<Vec<String>>().await {
                        Ok(urls_from_json) => {
                            info!("Successfully parsed JSON list from URL: {}", url_input_str);
                            let valid_urls: Vec<String> = urls_from_json.into_iter().filter(|u| u.starts_with("http://") || u.starts_with("https://")).collect();
                            if valid_urls.is_empty() {
                                warn!("JSON list from {} is empty or contains no valid HTTP/HTTPS URLs.", url_input_str);
                            }
                            return Ok(valid_urls);
                        }
                        Err(e) => {
                            warn!("Failed to parse response from {} as JSON list (Error: {}). Treating as a single URL.", url_input_str, e);
                        }
                    }
                } else {
                    warn!("HTTP request to {} failed with status {}. Treating as a single URL.", url_input_str, response.status());
                }
            }
            Err(e) => {
                warn!("Network error when trying to fetch {} as JSON list (Error: {}). Treating as a single URL.", url_input_str, e);
            }
        }
        if url_input_str.starts_with("http://") || url_input_str.starts_with("https://") {
            return Ok(vec![url_input_str.to_string()]);
        } else {
            error!("Invalid URL input: {}. Not a file, not a valid HTTP/S URL for a list, and not a single valid HTTP/S URL.", url_input_str);
            return Err(anyhow::anyhow!("Invalid URL input: {}", url_input_str));
        }
    }

    // If not a file and not an HTTP/S URL, it's an invalid input
    error!("Invalid input: '{}'. Not a recognized file path or HTTP/S URL.", url_input_str);
    Err(anyhow::anyhow!("Invalid input: {}. Must be a file path or an HTTP/S URL.", url_input_str))
}

#[tokio::main]
async fn main() -> Result<()> {
    let all_args: Vec<String> = env::args().collect();

    if all_args.iter().any(|arg| arg == "-h" || arg == "--help") {
        eprintln!("Usage: {} [URL_OR_FILE_OR_JSON_URL] [SAVE_DIR] [--concurrent <NUM>] [-c <NUM>] [--skip-head] [-s]", all_args.get(0).map_or("downloader", |s| s.as_str()));
        eprintln!("Arguments:");
        eprintln!("\tURL_OR_FILE_OR_JSON_URL: Single HTTP/HTTPS URL, path to a local file with HTTP/HTTPS URLs (one per line), or an HTTP/HTTPS URL to a JSON list of HTTP/HTTPS URLs.");
        eprintln!("\tSAVE_DIR (optional): Directory to save downloaded files. Defaults to './downloaded_files'.");
        eprintln!("\t--concurrent <NUM> or -c <NUM> (optional): Number of concurrent downloads. Defaults to 3. Must be > 0.");
        eprintln!("\t--skip-head or -s (optional): Skip HEAD request, extract filename from URL directly. Useful when HEAD requests fail or are slow.");
        eprintln!("\t-h, --help\tShow this help message");
        eprintln!("\t-v, --version\tShow version information");
        return Ok(());
    }

    if all_args.iter().any(|arg| arg == "-v" || arg == "--version") {
        eprintln!("Downloader version: {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let mut concurrent_downloads: usize = 3; // Reduced default concurrency
    let mut url_input_str: Option<String> = None;
    let mut save_dir_str: Option<String> = None;
    let mut skip_head: bool = false;

    // Parse command-line arguments
    let mut positional_arg_index = 0;
    let mut args_iter = all_args.iter().skip(1);
    while let Some(arg) = args_iter.next() {
        if arg == "--concurrent" || arg == "-c" {
            if let Some(val_str) = args_iter.next() {
                match val_str.parse::<usize>() {
                    Ok(num) if num > 0 => concurrent_downloads = num,
                    Ok(_) => eprintln!("--concurrent value must be a positive integer, using default {}.", concurrent_downloads),
                    Err(_) => eprintln!("Invalid number for --concurrent: '{}', using default {}.", val_str, concurrent_downloads),
                }
            } else {
                eprintln!("--concurrent option requires a value, using default {}.", concurrent_downloads);
            }
        } else if arg == "--skip-head" || arg == "-s" {
            skip_head = true;
        } else if !arg.starts_with('-') {
            if positional_arg_index == 0 {
                url_input_str = Some(arg.clone());
            } else if positional_arg_index == 1 {
                save_dir_str = Some(arg.clone());
            } else {
                eprintln!("Ignoring extra positional argument: {}", arg);
            }
            positional_arg_index += 1;
        } else {
            eprintln!("Ignoring unknown option: {}", arg);
        }
    }

    let url_input = match url_input_str {
        Some(s) => s,
        None => {
            eprintln!("Error: URL_OR_FILE_OR_JSON_URL is required. Use -h or --help for usage information.");
            return Err(anyhow::anyhow!("URL_OR_FILE_OR_JSON_URL is required."));
        }
    };

    let save_dir = match save_dir_str {
        Some(s) => PathBuf::from(s),
        None => env::current_dir()?.join("downloaded_files"),
    };

    if !save_dir.exists() {
        fs::create_dir_all(&save_dir)?;
    } else if !save_dir.is_dir() {
        eprintln!("Save path {:?} exists but is not a directory.", save_dir);
        return Err(anyhow::anyhow!("Save path {:?} is not a directory.", save_dir));
    }

    // Set up logging to save_dir
    let file_appender = rolling::daily(&save_dir, "vam_downloader.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);
    fmt()
        .with_writer(non_blocking)
        .init();

    let shared_client = Arc::new(Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30)) // Add timeout for HTTP requests
        .build()?);

    let urls_to_download = match resolve_url_input(&url_input, &shared_client).await {
        Ok(urls) => urls,
        Err(e) => {
            eprintln!("Failed to resolve input URLs: {}", e);
            return Err(e);
        }
    };

    if urls_to_download.is_empty() {
        eprintln!("No valid URLs to download.");
        return Ok(());
    }

    eprintln!("Preparing to download {} file(s) concurrently (max {}). Save directory: {:?}", urls_to_download.len(), concurrent_downloads, save_dir);

    let multi_progress = Arc::new(MultiProgress::new());
    let semaphore = Arc::new(Semaphore::new(concurrent_downloads));
    let mut download_handles = Vec::new();
    let mut url_handles = Vec::new();

    // Store URLs with their handles for error reporting
    for url_str in urls_to_download {
        let permit = Arc::clone(&semaphore).acquire_owned().await?;
        let client_clone = Arc::clone(&shared_client);
        let save_dir_clone = save_dir.clone();
        let mp_clone = Arc::clone(&multi_progress);
        let pb = mp_clone.add(ProgressBar::new(0));
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{msg} {bar:40.cyan/blue} {percent}% ({bytes}/{total_bytes})")?
                .progress_chars("#>-")
        );
        let pb = Arc::new(pb);
        let url_str_clone = url_str.clone();
        let skip_head_clone = skip_head;
        let task = tokio::spawn(async move {
            let download_result = download_file_task(url_str_clone, save_dir_clone, client_clone, pb, skip_head_clone).await;
            drop(permit);
            download_result
        });
        download_handles.push(task);
        url_handles.push(url_str);
    }

    let mut successful_downloads = 0;
    let mut failed_downloads = 0;
    let mut failed_urls = Vec::new();

    // Process task results
    for (handle, url) in download_handles.into_iter().zip(url_handles.into_iter()) {
        match handle.await {
            Ok(Ok(_)) => successful_downloads += 1,
            Ok(Err(e)) => {
                failed_downloads += 1;
                failed_urls.push((url, e.to_string()));
            }
            Err(join_error) => {
                failed_downloads += 1;
                failed_urls.push((url, format!("Task panicked: {}", join_error)));
            }
        }
    }

    eprintln!("Batch download summary: {} successful, {} failed.", successful_downloads, failed_downloads);
    if !failed_urls.is_empty() {
        eprintln!("Failed downloads:");
        for (url, error) in failed_urls {
            eprintln!("- {}: {}", url, error);
        }
    }

    if failed_downloads > 0 {
        return Err(anyhow::anyhow!("{} download(s) failed.", failed_downloads));
    }

    Ok(())
}