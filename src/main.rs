use reqwest::{Client, header};
use tokio;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let url = "https://hub.virtamate.com/resources/30051/version/38995/download?file=212015";
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

    if response.status() == reqwest::StatusCode::SEE_OTHER {
        if let Some(location) = response.headers().get(reqwest::header::LOCATION) {
            let final_url = location.to_str()?;
            println!("重定向后的URL: {}", final_url);
        }
    }

    // DEBUG 打印JSON格式化的完整响应
    // let status = response.status();
    // let headers = response.headers().clone();
    // let response_data = json!({
    //     "status_code": status.as_u16(),
    //     "headers": headers.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap())).collect::<Value>(),
    //     "url": response.url().to_string()
    // });
    // println!("{}", serde_json::to_string_pretty(&response_data)?);

    Ok(())
}
