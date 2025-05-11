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
