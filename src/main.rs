use std::num::{NonZeroU8, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::env;
use std::fs;
use anyhow::Result;
use tracing::{info, error, warn};
use url::Url;
use reqwest::{Client, header};
use http_downloader::{HttpDownloaderBuilder, speed_tracker::DownloadSpeedTrackerExtension, status_tracker::DownloadStatusTrackerExtension, DownloadingEndCause};
use http_downloader::speed_limiter::DownloadSpeedLimiterExtension;
use regex::Regex;
use percent_encoding::percent_decode;
use tokio::sync::Semaphore;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use tokio::time::timeout;

async fn download_file_task(
    url_to_download: String,
    save_dir: PathBuf,
    client: Arc<Client>,
) -> Result<()> {
    info!("Starting download for: {}", url_to_download);

    // Validate URL
    if url_to_download.is_empty() || (!url_to_download.starts_with("http://") && !url_to_download.starts_with("https://")) {
        error!("Invalid URL '{}'. Please provide a valid http/https URL.", url_to_download);
        return Err(anyhow::anyhow!("Invalid URL: {}. Must be http/https.", url_to_download));
    }

    // Resolve final URL (handles redirects)
    let final_url_str = if url_to_download.ends_with(".data") {
        url_to_download.clone()
    } else {
        let mut headers = header::HeaderMap::new();
        headers.insert(header::ACCEPT, "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7".parse()?);
        headers.insert(header::ACCEPT_ENCODING, "gzip, deflate, br, zstd".parse()?);
        headers.insert(header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9".parse()?);
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
        if response.status() == reqwest::StatusCode::SEE_OTHER {
            if let Some(location) = response.headers().get(header::LOCATION) {
                location.to_str()?.to_string()
            } else {
                warn!("No redirect location found for {}, using original.", url_to_download);
                url_to_download.clone()
            }
        } else {
            url_to_download.clone()
        }
    };

    // Sanitize filename
    let filename = {
        let head_response = client.head(&final_url_str).send().await?;
        let content_disposition = head_response.headers().get(header::CONTENT_DISPOSITION);

        let mut extracted_filename = if let Some(cd_val) = content_disposition {
            let cd_str = percent_decode(cd_val.as_bytes()).decode_utf8().unwrap_or_else(|_| "".into());
            let re = Regex::new(r#"filename(?:\*)?=(?:"([^"]+)"|([^;\s]+))(?:;.*)?$"#)?;
            if let Some(captures) = re.captures(&cd_str) {
                captures.get(1).or(captures.get(2)).map(|m| m.as_str().to_string())
                    .unwrap_or_else(|| "default_filename".to_string())
            } else {
                Path::new(&final_url_str)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| "default_filename".to_string())
            }
        } else {
            Path::new(&final_url_str)
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "default_filename".to_string())
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

    let download_url_obj = Url::parse(&final_url_str)?;

    // Download with retry logic
    const MAX_RETRIES: u8 = 1;
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
            let download_future = downloader.prepare_download()
                .map_err(|e| anyhow::anyhow!("Failed to prepare download: {}", e))?;

            let progress_handle = tokio::spawn({
                let mut downloaded_len_receiver = downloader.downloaded_len_receiver().clone();
                let total_size_future = downloader.total_size_future();
                let p_filename = filename.clone();
                async move {
                    let total_len = total_size_future.await;
                    if let Some(total_len) = total_len {
                        if total_len.get() > 0 {
                            info!("[{}] Total size: {:.2} Mb", p_filename, total_len.get() as f64 / 1024_f64 / 1024_f64);
                        } else {
                            info!("[{}] Total size: 0 bytes or unknown", p_filename);
                        }
                    }
                    while downloaded_len_receiver.changed().await.is_ok() {
                        let progress = *downloaded_len_receiver.borrow();
                        if let Some(total_len) = total_len {
                            if total_len.get() > 0 {
                                info!("[{}] Progress: {} % ({}/{} bytes)", p_filename, progress * 100 / total_len, progress, total_len);
                            } else if progress > 0 {
                                info!("[{}] Progress: {} bytes (total size 0 or unknown)", p_filename, progress);
                            }
                        } else {
                            info!("[{}] Progress: {} bytes (total size unknown)", p_filename, progress);
                        }
                        tokio::time::sleep(Duration::from_millis(1000)).await;
                    }
                }
            });

            info!("[{}] Start downloading until the end (attempt {})", filename, attempt);
            let dec = download_future.await
                .map_err(|e| anyhow::anyhow!("Download failed: {}", e))?;
            progress_handle.abort();
            Ok::<DownloadingEndCause, anyhow::Error>(dec)
        }).await;

        match result {
            Ok(Ok(dec)) => break Ok(dec),
            Ok(Err(e)) => {
                if attempt <= MAX_RETRIES && e.to_string().to_lowercase().contains("error sending request") {
                    warn!("[{}] Download failed: {}. Retrying (attempt {}/{})", filename, e, attempt +1 , MAX_RETRIES +1); // attempt is already incremented for next
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    continue;
                } else {
                    error!("[{}] Download failed after {} attempts: {}", filename, attempt, e);
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
                    break Err(anyhow::anyhow!("Download timed out after {} attempts", attempt));
                }
            }
        }
    };

    let dec = download_result?;
    info!("[{}] Downloading end cause: {:?}", filename, dec);

    // Verify and rename file
    let downloaded_file_path = save_dir.join(download_url_obj.path_segments().and_then(|s| s.last()).unwrap_or("unknown_temp_file"));
    let new_file_path = save_dir.join(&filename);

    if downloaded_file_path == new_file_path {
        info!("[{}] File already at target location: {:?}", filename, new_file_path);
    } else if downloaded_file_path.exists() {
        let file_size = fs::metadata(&downloaded_file_path)?.len();
        if file_size == 0 {
            error!("[{}] Downloaded file is empty: {:?}", filename, downloaded_file_path);
            return Err(anyhow::anyhow!("Downloaded file {} is empty", filename));
        }

        match fs::rename(&downloaded_file_path, &new_file_path) {
            Ok(_) => info!("[{}] File renamed to: {:?}", filename, new_file_path),
            Err(e) => {
                error!("[{}] File rename failed from {:?} to {:?}: {}", filename, downloaded_file_path, new_file_path, e);
                return Err(anyhow::anyhow!("File rename failed for {}: {}", filename, e));
            }
        }
    } else {
        warn!("[{}] Expected downloaded file not found at {:?}. Checking target location {:?}", filename, downloaded_file_path, new_file_path);
        if !new_file_path.exists() {
            error!("[{}] Final file {:?} not found. Download may have failed silently.", filename, new_file_path);
            return Err(anyhow::anyhow!("Final file {} not found after download.", filename));
        } else {
            let file_size = fs::metadata(&new_file_path)?.len();
            if file_size == 0 {
                error!("[{}] Downloaded file at target location is empty: {:?}", filename, new_file_path);
                return Err(anyhow::anyhow!("Downloaded file {} is empty", filename));
            }
            info!("[{}] File found at target location: {:?}", filename, new_file_path);
        }
    }

    Ok(())
}

async fn resolve_url_input(url_input_str: &str, client: &Arc<Client>) -> Result<Vec<String>> {
    let path = PathBuf::from(url_input_str);
    // Check if it's a file first
    if fs::metadata(&path).is_ok() && path.is_file() {
        info!("Input is a local file: {}", url_input_str);
        let content = fs::read_to_string(path)?;
        let urls: Vec<String> = content.lines().map(str::trim).filter(|s| !s.is_empty() && (s.starts_with("http://") || s.starts_with("https://"))).map(String::from).collect();
        if urls.is_empty() {
            warn!("File {} is empty or contains no valid HTTP/HTTPS URLs.", url_input_str);
        }
        return Ok(urls);
    }

    // If not a file, check if it's an HTTP/S URL (could be single or JSON list)
    if url_input_str.starts_with("http://") || url_input_str.starts_with("https://") {
        info!("Input is a URL: {}. Checking if it's a JSON list.", url_input_str);
        // Try to fetch as JSON list
        match client.get(url_input_str).send().await {
            Ok(response) => {
                if response.status().is_success() {
                    // Attempt to parse as Vec<String>
                    match response.json::<Vec<String>>().await {
                        Ok(urls_from_json) => {
                            info!("Successfully parsed JSON list from URL: {}", url_input_str);
                            let valid_urls: Vec<String> = urls_from_json.clone().into_iter().filter(|u| u.starts_with("http://") || u.starts_with("https://")).collect();
                            if valid_urls.is_empty() && !urls_from_json.is_empty() {
                                warn!("JSON list from {} contained URLs, but none were valid HTTP/HTTPS.", url_input_str);
                            } else if valid_urls.is_empty() {
                                warn!("JSON list from {} is empty or contains no valid HTTP/HTTPS URLs.", url_input_str);
                            }
                            return Ok(valid_urls);
                        }
                        Err(e) => {
                            // Not a JSON list, or JSON structure is wrong. Treat as a single URL.
                            warn!("Failed to parse response from {} as JSON list (Error: {}). Treating as a single URL.", url_input_str, e);
                        }
                    }
                } else {
                    // HTTP request failed. Treat as a single URL (it will likely fail in download_file_task).
                    warn!("HTTP request to {} failed with status {}. Treating as a single URL.", url_input_str, response.status());
                }
            }
            Err(e) => {
                // Network error. Treat as a single URL.
                warn!("Network error when trying to fetch {} as JSON list (Error: {}). Treating as a single URL.", url_input_str, e);
            }
        }
        // If all attempts to parse as JSON list fail, or if it's not a JSON list, treat as a single URL.
        // Validate if the single URL itself is http/https
        if url_input_str.starts_with("http://") || url_input_str.starts_with("https://") {
            return Ok(vec![url_input_str.to_string()]);
        } else {
            // This case should ideally not be reached if the initial check for http/https was done,
            // but as a fallback.
            error!("Invalid URL input: {}. Not a file, not a valid HTTP/S URL for a list, and not a single valid HTTP/S URL.", url_input_str);
            return Err(anyhow::anyhow!("Invalid URL input: {}", url_input_str));
        }
    }

    // If not a file and not an HTTP/S URL, it's an invalid input.
    error!("Invalid input: '{}'. Not a recognized file path or HTTP/S URL.", url_input_str);
    Err(anyhow::anyhow!("Invalid input: {}. Must be a file path or an HTTP/S URL.", url_input_str))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let all_args: Vec<String> = env::args().collect();

    if all_args.iter().any(|arg| arg == "-h" || arg == "--help") {
        let program_name = all_args.get(0).map_or("downloader", |s| s.as_str());
        info!("Usage: {} [URL_OR_FILE_OR_JSON_URL] [SAVE_DIR] [--concurrent <NUM>] [-c <NUM>]", program_name);
        info!("Arguments:");
        info!("\tURL_OR_FILE_OR_JSON_URL: Single HTTP/HTTPS URL, path to a local file with HTTP/HTTPS URLs (one per line), or an HTTP/HTTPS URL to a JSON list of HTTP/HTTPS URLs.");
        info!("\tSAVE_DIR (optional): Directory to save downloaded files. Defaults to './downloaded_files'.");
        info!("\t--concurrent <NUM> or -c <NUM> (optional): Number of concurrent downloads. Defaults to 3. Must be > 0.");
        info!("\t-h, --help\tShow this help message");
        info!("\t-v, --version\tShow version information");
        return Ok(());
    }

    if all_args.iter().any(|arg| arg == "-v" || arg == "--version") {
        info!("Downloader version: {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    let mut concurrent_downloads: usize = 3;
    let mut url_input_str: Option<String> = None;
    let mut save_dir_str: Option<String> = None;
    
    // Simple positional argument parsing for now.
    // Assumes: <URL_OR_FILE_OR_JSON_URL> [SAVE_DIR] followed by options.
    // A more robust parser like `clap` would be better for complex scenarios.
    let mut positional_arg_index = 0;
    let mut args_iter = all_args.iter().skip(1); // Skip program name

    while let Some(arg) = args_iter.next() {
        if arg == "--concurrent" || arg == "-c" {
            if let Some(val_str) = args_iter.next() {
                match val_str.parse::<usize>() {
                    Ok(num) if num > 0 => concurrent_downloads = num,
                    Ok(_) => {
                        warn!("--concurrent value must be a positive integer, using default {}.", concurrent_downloads);
                        // Potentially exit or use default. Here, using default.
                    }
                    Err(_) => {
                        warn!("Invalid number for --concurrent: '{}', using default {}.", val_str, concurrent_downloads);
                    }
                }
            } else {
                warn!("--concurrent option requires a value, using default {}.", concurrent_downloads);
                // Potentially exit or use default.
            }
        } else if !arg.starts_with('-') { // Positional argument
            if positional_arg_index == 0 {
                url_input_str = Some(arg.clone());
            } else if positional_arg_index == 1 {
                save_dir_str = Some(arg.clone());
            } else {
                warn!("Ignoring extra positional argument: {}", arg);
            }
            positional_arg_index += 1;
        } else {
            // Unknown option
            warn!("Ignoring unknown option: {}", arg);
        }
    }


    let url_input = match url_input_str {
        Some(s) => s,
        None => {
            error!("Error: URL_OR_FILE_OR_JSON_URL is required. Use -h or --help for usage information.");
            return Err(anyhow::anyhow!("URL_OR_FILE_OR_JSON_URL is required."));
        }
    };

    let save_dir = match save_dir_str {
        Some(s) => PathBuf::from(s),
        None => env::current_dir()?.join("downloaded_files"), // Changed default dir name
    };

    if !save_dir.exists() {
        info!("Save directory does not exist, creating: {:?}", save_dir);
        fs::create_dir_all(&save_dir)?;
    } else if !save_dir.is_dir() {
        error!("Save path {:?} exists but is not a directory.", save_dir);
        return Err(anyhow::anyhow!("Save path {:?} is not a directory.", save_dir));
    }


    let shared_client = Arc::new(Client::builder()
        .redirect(reqwest::redirect::Policy::none()) // Keep redirect policy for client
        .build()?);

    let urls_to_download = match resolve_url_input(&url_input, &shared_client).await {
        Ok(urls) => urls,
        Err(e) => {
            error!("Failed to resolve input URLs: {}", e);
            return Err(e);
        }
    };

    if urls_to_download.is_empty() {
        info!("No valid URLs to download.");
        return Ok(());
    }

    info!("Preparing to download {} file(s) concurrently (max {}). Save directory: {:?}", urls_to_download.len(), concurrent_downloads, save_dir);

    let semaphore = Arc::new(Semaphore::new(concurrent_downloads));
    let mut download_handles = Vec::new();

    for url_str in urls_to_download {
        let permit = Arc::clone(&semaphore).acquire_owned().await?; // acquire_owned is correct
        let client_clone = Arc::clone(&shared_client);
        let save_dir_clone = save_dir.clone();
        
        let task = tokio::spawn(async move {
            let download_result = download_file_task(url_str, save_dir_clone, client_clone).await;
            drop(permit); // Release semaphore permit when task finishes (success or error)
            download_result
        });
        download_handles.push(task);
    }

    let mut successful_downloads = 0;
    let mut failed_downloads = 0;

    for handle in download_handles {
        match handle.await { // This waits for the tokio task to complete
            Ok(Ok(_)) => { // Task completed, and download_file_task returned Ok
                successful_downloads += 1;
            }
            Ok(Err(e)) => { // Task completed, but download_file_task returned Err
                // Error should have been logged by download_file_task, but we can log a summary error here
                error!("A download task failed: {}", e);
                failed_downloads += 1;
            }
            Err(join_error) => { // Task panicked
                error!("A download task panicked: {}", join_error);
                failed_downloads += 1;
            }
        }
    }

    info!("Batch download summary: {} successful, {} failed.", successful_downloads, failed_downloads);

    if failed_downloads > 0 {
        return Err(anyhow::anyhow!("{} download(s) failed.", failed_downloads));
    }

    Ok(())
}
