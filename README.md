# VAM Downloader

## TL;DR

This is a download plugin which is embedded in [github.com/bustesoul/varManager](https://github.com/bustesoul/varManager). DO NOT use it standalone until you have fully read and understood the source code.

## Usage

```powershell
$CMD[powershell]
.\vam_downloader.exe [URL_OR_FILE_OR_JSON_URL] {[SAVE_DIR]} [--concurrent <NUM>] [-c <NUM>]
    URL_OR_FILE_OR_JSON_URL: Single HTTP/HTTPS URL, 
                             path to a local file with HTTP/HTTPS URLs (one per line), 
                             or an HTTP/HTTPS URL to a JSON list of HTTP/HTTPS URLs.
    SAVE_DIR (optional):     Directory to save downloaded files. Defaults to './downloaded_files'.
    --concurrent <NUM> or 
    -c <NUM> (optional):     Number of concurrent downloads. Defaults to 3. Must be > 0.
    -h, --help:              Show this help message
    -v, --version:           Show version information
```

## Main Features

* **Batch Download**: Support batch download from a single URL, local text file (one URL per line) or remote JSON list URL.
* **Concurrent Download**: You can specify the number of files to download simultaneously via `--concurrent` or `-c` parameter, the default is 3.
* **Progress Display**:
  * When downloading multiple files, a separate progress bar will be displayed for each concurrent download task.
  * Clearly display the download progress, downloaded size and total size of each file.
* **Logging**:
  * Detailed download process logs are automatically saved to the `vam_downloader.log` file in the specified download directory (default is `./downloaded_files`).
  * Log files are rotated daily.
* **Smart Filename Parsing**: Try to extract and clean file names from the `Content-Disposition` HTTP header or URL path.
* **Error Handling and Retry**:
  * Automatically retry certain retryable errors (such as connection problems, timeouts) during network requests and downloads.
  * After downloading is complete, it will check whether the file is empty.
