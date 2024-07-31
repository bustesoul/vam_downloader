use std::num::{NonZeroU8, NonZeroUsize};
use std::path::PathBuf;
use std::time::Duration;
use std::env;

use anyhow::Result;
use tracing::info;
use url::Url;
use reqwest::{Client, header};
use serde_json::{json, Value};

use http_downloader::{
    breakpoint_resume::DownloadBreakpointResumeExtension,
    HttpDownloaderBuilder,
    speed_tracker::DownloadSpeedTrackerExtension,
    status_tracker::DownloadStatusTrackerExtension,
};
use http_downloader::bson_file_archiver::{ArchiveFilePath, BsonFileArchiverBuilder};
use http_downloader::speed_limiter::DownloadSpeedLimiterExtension;

#[tokio::main]
async fn main() -> Result<()> {
    // 初始化日志记录
    tracing_subscriber::fmt::init();

    // 从命令行参数获取下载 URL 和保存路径
    let args: Vec<String> = env::args().collect();
    let url = if args.len() > 1 {
        &args[1]
    } else {
        "https://hub.virtamate.com/resources/clara-the-minotaur.49186/download"
    };
    let save_dir = if args.len() > 2 {
        PathBuf::from(&args[2])
    } else {
        env::current_dir()?.join("downloaded_file.var")
    };

    // 使用 reqwest 获取真实下载链接
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none()) // 禁用自动重定向
        .build()?;

    let mut headers = header::HeaderMap::new();
    headers.insert(header::ACCEPT, "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,image/apng,*/*;q=0.8,application/signed-exchange;v=b3;q=0.7".parse().unwrap());
    headers.insert(header::ACCEPT_ENCODING, "gzip, deflate, br, zstd".parse().unwrap());
    headers.insert(header::ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9".parse().unwrap());
    headers.insert(header::COOKIE, "vamhubconsent=yes".parse().unwrap());
    headers.insert(header::DNT, "1".parse().unwrap());
    headers.insert(header::HeaderName::from_static("sec-ch-ua"), "\"Not)A;Brand\";v=\"99\", \"Microsoft Edge\";v=\"127\", \"Chromium\";v=\"127\"".parse().unwrap());
    headers.insert(header::HeaderName::from_static("sec-ch-ua-mobile"), "?0".parse().unwrap());
    headers.insert(header::HeaderName::from_static("sec-ch-ua-platform"), "\"Windows\"".parse().unwrap());
    headers.insert(header::HeaderName::from_static("sec-fetch-dest"), "document".parse().unwrap());
    headers.insert(header::HeaderName::from_static("sec-fetch-mode"), "navigate".parse().unwrap());
    headers.insert(header::HeaderName::from_static("sec-fetch-site"), "none".parse().unwrap());
    headers.insert(header::HeaderName::from_static("sec-fetch-user"), "?1".parse().unwrap());
    headers.insert(header::UPGRADE_INSECURE_REQUESTS, "1".parse().unwrap());
    headers.insert(header::USER_AGENT, "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/127.0.0.0 Safari/537.36 Edg/127.0.0.0".parse().unwrap());

    let response = client.get(url).headers(headers).send().await?;

    let final_url = if response.status() == reqwest::StatusCode::SEE_OTHER {
        if let Some(location) = response.headers().get(header::LOCATION) {
            location.to_str()?.to_string()
        } else {
            return Err(anyhow::anyhow!("未找到重定向链接"));
        }
    } else {
        url.to_string()
    };

    // 使用 http_downloader 进行多线程和断点续传下载
    let test_url = Url::parse(&final_url)?;

    let (mut downloader, (status_state, speed_state, speed_limiter, ..)) =
        HttpDownloaderBuilder::new(test_url, save_dir)
            .chunk_size(NonZeroUsize::new(1024 * 1024 * 10).unwrap()) // 块大小
            .download_connection_count(NonZeroU8::new(3).unwrap())
            .build((
                // 下载状态追踪扩展
                DownloadStatusTrackerExtension { log: true },
                // 下载速度追踪扩展
                DownloadSpeedTrackerExtension { log: true },
                // 下载速度限制扩展，
                DownloadSpeedLimiterExtension::new(None),
                // 断点续传扩展，
                DownloadBreakpointResumeExtension {
                    // BsonFileArchiver by cargo feature "bson-file-archiver" enable
                    download_archiver_builder: BsonFileArchiverBuilder::new(ArchiveFilePath::Suffix("bson".to_string()))
                }
            ));
    info!("Prepare download，准备下载");
    let download_future = downloader.prepare_download()?;

    let _status = status_state.status(); // 获取下载状态
    let _status_receiver = status_state.status_receiver; // 状态监听器
    let _byte_per_second = speed_state.download_speed(); // 获取下载速度，字节每秒
    let _speed_receiver = speed_state.receiver; // 获取下载速度监听器

    // downloader.cancel() // 取消下载

    // 打印下载进度
    tokio::spawn({
        let mut downloaded_len_receiver = downloader.downloaded_len_receiver().clone();
        let total_size_future = downloader.total_size_future();
        async move {
            let total_len = total_size_future.await;
            if let Some(total_len) = total_len {
                info!("Total size: {:.2} Mb", total_len.get() as f64 / 1024_f64 / 1024_f64);
            }
            while downloaded_len_receiver.changed().await.is_ok() {
                let progress = *downloaded_len_receiver.borrow();
                if let Some(total_len) = total_len {
                    info!("Download Progress: {} %，{}/{}", progress * 100 / total_len, progress, total_len);
                }

                tokio::time::sleep(Duration::from_millis(1000)).await;
            }
        }
    });

    // 下载速度限制
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(2)).await;
        info!("Start speed limit，开始限速");
        speed_limiter.change_speed(Some(1024 * 1024 * 2)).await;
        tokio::time::sleep(Duration::from_secs(4)).await;
        info!("Remove the download speed limit，解除速度限制");
        speed_limiter.change_speed(None).await;
    });

    info!("Start downloading until the end，开始下载直到结束");
    let dec = download_future.await?;
    info!("Downloading end cause: {:?}", dec);
    Ok(())
}
