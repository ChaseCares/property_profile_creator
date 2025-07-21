#![warn(
    clippy::all,
    clippy::pedantic,
    missing_debug_implementations,
    unsafe_code,
    unused_extern_crates,
    unused_import_braces,
    unused_qualifications,
    trivial_casts,
    trivial_numeric_casts
)]

use std::{
    env,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use actix_web::{App, HttpResponse, HttpServer, Responder, web};
use futures_util::StreamExt;
use num_format::{Locale, ToFormattedString};
use once_cell::sync::Lazy;
use regex::Regex;
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::{
    fs::{self, File},
    io::AsyncWriteExt,
    sync::mpsc,
};
use tokio_stream::wrappers::ReceiverStream;

#[derive(Error, Debug)]
enum AppError {
    #[error("Network request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("File I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Failed to parse data: {0}")]
    Parse(String),
    #[error("Configuration error: {0}")]
    Config(#[from] env::VarError),
    #[error("Nextcloud server error: {status} - {body}")]
    Nextcloud { status: StatusCode, body: String },
    #[error("Failed to serialize data: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Debug)]
struct Config {
    output_dir: String,
    download_delay_secs: u64,
    skip_images: bool,
    nc_url: String,
    nc_user: String,
    nc_pass: String,
}

impl Config {
    fn from_env() -> Result<Self, AppError> {
        dotenvy::dotenv().ok();

        Ok(Self {
            output_dir: env::var("OUTPUT_DIR").unwrap_or_else(|_| "output".to_string()),
            download_delay_secs: env::var("DOWNLOAD_DELAY_SECS")
                .unwrap_or_else(|_| "2".to_string())
                .parse()
                .unwrap_or(2),
            skip_images: env::var("SKIP_IMAGES").is_ok(),
            nc_url: env::var("NC_URL").map_err(AppError::Config)?,
            nc_user: env::var("NC_USERNAME").map_err(AppError::Config)?,
            nc_pass: env::var("NC_PASSWORD").map_err(AppError::Config)?,
        })
    }
}

static IMAGE_LINK_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#""(https://[^"]*?origin\.webp)""#).unwrap());
static ST_CITY_STATE_ZIP_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"<title>(.*?), (.*?), (..) (\d{5}) \| MLS #(\d*?) \| Compass</title>").unwrap()
});
static PRICE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"propertyHistory-table-td.><div>\$([0-9,]+)</div></td></tr>").unwrap()
});

static DESCRIPTION_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r".,.addressCountry.:.US.}},.description.:.(.+?).,.floorSize.:").unwrap()
});

#[derive(Serialize, Debug)]
struct PropertyMetadata {
    url: String,
    street: String,
    city: String,
    state: String,
    zip: String,
    mls: String,
    price: u32,
    description: String,
    #[serde(skip)]
    image_links: Vec<String>,
}

impl PropertyMetadata {
    fn full_address(&self) -> String {
        format!(
            "{}, {}, {} {}",
            self.street, self.city, self.state, self.zip
        )
    }
}

#[derive(Debug)]
struct Scraper {
    client: Client,
}

impl Scraper {
    fn new() -> Result<Self, reqwest::Error> {
        let client = Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/125.0.0.0 Safari/537.36")
            .timeout(Duration::from_secs(30))
            .build()?;
        Ok(Self { client })
    }

    async fn fetch_property_data(
        &self,
        url: &str,
        tx: mpsc::Sender<String>,
    ) -> Result<PropertyMetadata, AppError> {
        println!("Fetching HTML from {url}...");
        let _ = tx.send(format!("Fetching HTML from {url}...\n")).await;
        let html = self.client.get(url).send().await?.text().await?;

        let st_city_state_zip_caps = ST_CITY_STATE_ZIP_RE.captures(&html).ok_or_else(|| {
            AppError::Parse("Could not extract address and MLS info from HTML title".to_string())
        })?;

        let price_str = PRICE_RE
            .captures(&html)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().replace(',', ""))
            .ok_or_else(|| AppError::Parse("Could not extract price".to_string()))?;

        let description = DESCRIPTION_RE
            .captures(&html)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str())
            .ok_or_else(|| AppError::Parse("Could not extract property description".to_string()))?;

        let image_links = IMAGE_LINK_RE
            .captures_iter(&html)
            .map(|cap| cap[1].to_string())
            .collect();

        Ok(PropertyMetadata {
            url: url.to_string(),
            street: st_city_state_zip_caps[1].to_string(),
            city: st_city_state_zip_caps[2].to_string(),
            state: st_city_state_zip_caps[3].to_string(),
            zip: st_city_state_zip_caps[4].to_string(),
            mls: st_city_state_zip_caps[5].to_string(),
            price: price_str
                .parse()
                .map_err(|_| AppError::Parse(format!("Failed to parse price: {price_str}")))?,
            description: description.to_string(),
            image_links,
        })
    }

    async fn download_images(
        &self,
        metadata: &PropertyMetadata,
        images_dir: &Path,
        delay: u64,
        tx: mpsc::Sender<String>,
    ) -> Result<(), AppError> {
        println!(
            "Downloading {} images sequentially...",
            metadata.image_links.len()
        );
        let _ = tx
            .send(format!(
                "Downloading {} images sequentially...",
                metadata.image_links.len()
            ))
            .await;
        for (i, link) in metadata.image_links.iter().enumerate() {
            let file_name = format!("{}.webp", i + 1);
            let file_path = images_dir.join(file_name);
            println!("Downloading {link}...");
            let _ = tx.send(format!("Downloading {link}...\n")).await;

            if file_path.exists() {
                println!(" -> File already exists. Skipping.");
                let _ = tx
                    .send(" -> File already exists. Skipping.".to_string())
                    .await;
                continue;
            }

            let response = self.client.get(link).send().await?.error_for_status()?;
            let content = response.bytes().await?;
            fs::write(&file_path, &content).await?;

            println!(" -> Saved to {}", file_path.display());
            let _ = tx
                .send(format!(" -> Saved to {}", file_path.display()))
                .await;
            tokio::time::sleep(Duration::from_secs(delay)).await;
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
struct NextcloudClient {
    client: Client,
    base_url: String,
    username: String,
    password: String,
}

impl NextcloudClient {
    #[must_use]
    fn new(base_url: &str, username: &str, password: &str) -> Self {
        Self {
            client: Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            username: username.to_string(),
            password: password.to_string(),
        }
    }

    fn build_dav_url(&self, remote_path: &str) -> String {
        format!(
            "{}/remote.php/dav/files/{}/{}",
            self.base_url,
            self.username,
            remote_path.trim_start_matches('/')
        )
    }

    async fn check_response(response: reqwest::Response) -> Result<reqwest::Response, AppError> {
        if response.status().is_success() {
            Ok(response)
        } else {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            Err(AppError::Nextcloud { status, body })
        }
    }

    async fn upload(
        &self,
        local_path: &Path,
        remote_path: &str,
        tx: mpsc::Sender<String>,
    ) -> Result<(), AppError> {
        let url = self.build_dav_url(remote_path);
        let file_contents = fs::read(local_path).await?;
        println!("Uploading '{}' to '{}'", local_path.display(), remote_path);
        let _ = tx
            .send(format!(
                "Uploading '{}' to '{}'",
                local_path.display(),
                remote_path
            ))
            .await;

        let response = self
            .client
            .put(&url)
            .basic_auth(&self.username, Some(&self.password))
            .body(file_contents)
            .send()
            .await?;
        Self::check_response(response).await?;
        Ok(())
    }

    async fn download(
        &self,
        remote_path: &str,
        local_path: &Path,
        tx: mpsc::Sender<String>,
    ) -> Result<(), AppError> {
        let url = self.build_dav_url(remote_path);
        println!("Downloading '{remote_path}' to '{}'", local_path.display());
        let _ = tx
            .send(format!(
                "Downloading '{remote_path}' to '{}'",
                local_path.display()
            ))
            .await;

        let response = self
            .client
            .get(&url)
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await?;

        let successful_response = Self::check_response(response).await?;
        let mut file = File::create(local_path).await?;
        let mut stream = successful_response.bytes_stream();

        while let Some(item) = stream.next().await {
            file.write_all(&item?).await?;
        }
        Ok(())
    }

    async fn create_folder_recursive(
        &self,
        folder_path: &str,
        tx: mpsc::Sender<String>,
    ) -> Result<(), AppError> {
        let mut current_path = PathBuf::new();
        for component in Path::new(folder_path).components() {
            current_path.push(component);
            if let Some(path_str) = current_path.to_str() {
                let tx_clone = tx.clone();
                self.create_folder(path_str, tx_clone).await?;
            }
        }
        Ok(())
    }

    async fn create_folder(
        &self,
        folder_path: &str,
        tx: mpsc::Sender<String>,
    ) -> Result<(), AppError> {
        println!("Ensuring folder exists at '{folder_path}'");
        let _ = tx
            .send(format!("Ensuring folder exists at '{folder_path}'"))
            .await;

        let response = self
            .client
            .request(
                Method::from_bytes(b"MKCOL").unwrap(),
                self.build_dav_url(folder_path),
            )
            .basic_auth(&self.username, Some(&self.password))
            .send()
            .await?;

        let status = response.status();
        // 201 Created: Success
        // 405 Method Not Allowed: Folder already exists, which is ok.
        if status.is_success() || status == StatusCode::METHOD_NOT_ALLOWED {
            Ok(())
        } else {
            Self::check_response(response).await.map(|_| ())
        }
    }
}

async fn create_listing(url: String, tx: mpsc::Sender<String>) -> Result<(), AppError> {
    let config = Config::from_env()?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string();

    let base_dir = PathBuf::from(&config.output_dir).join(&timestamp);
    let images_dir = base_dir.join("images");
    fs::create_dir_all(&images_dir).await?;

    let scraper = Scraper::new()?;
    let tx_clone = tx.clone();
    let metadata = scraper.fetch_property_data(&url, tx_clone).await?;
    let metadata_path = base_dir.join("metadata.json");
    fs::write(&metadata_path, serde_json::to_string_pretty(&metadata)?).await?;
    println!("Metadata saved to {}", metadata_path.display());
    let _ = tx
        .send(format!("Metadata saved to {}", metadata_path.display()))
        .await;

    let html_path = base_dir.join("page.html");
    let html = scraper.client.get(&url).send().await?.text().await?;
    fs::write(&html_path, html).await?;
    println!("HTML saved to {}", html_path.display());
    let _ = tx
        .send(format!("HTML saved to {}", html_path.display()))
        .await;

    if config.skip_images {
        println!("--skip-images flag is set, skipping download.");
        let _ = tx.send("Image download skipped.\n".to_string()).await;
    } else if !metadata.image_links.is_empty() {
        let tx_clone = tx.clone();
        scraper
            .download_images(&metadata, &images_dir, config.download_delay_secs, tx_clone)
            .await?;
        println!("Image download complete.");
        let _ = tx.send("Image download complete.\n".to_string()).await;
    }

    let nc_client = NextcloudClient::new(&config.nc_url, &config.nc_user, &config.nc_pass);
    let remote_base_path = "Propertes"; // TODO: Make this configurable
    let remote_property_dir = format!("{}/{}", remote_base_path, metadata.street);
    let remote_images_dir = format!("{remote_property_dir}/images/compass");

    let tx_clone = tx.clone();
    nc_client
        .create_folder_recursive(&remote_images_dir, tx_clone)
        .await?;

    for (i, _) in metadata.image_links.iter().enumerate() {
        let file_name = format!("{}.webp", i + 1);
        let local_path = images_dir.join(&file_name);
        let remote_path = format!("{remote_images_dir}/{file_name}");

        let tx_clone = tx.clone();
        if let Err(e) = nc_client.upload(&local_path, &remote_path, tx_clone).await {
            eprintln!("Could not upload {file_name}: {e:?}");
            let _ = tx
                .send(format!("Failed to upload {file_name}: {e}\n"))
                .await;
        }
    }
    println!("Image upload complete.");
    let _ = tx.send("Image upload complete.\n".to_string()).await;

    let template_remote_path = "Templates/new-property.md";
    let template_local_path = base_dir.join("info.md");
    let tx_clone = tx.clone();
    nc_client
        .download(template_remote_path, &template_local_path, tx_clone)
        .await?;

    let mut contents = fs::read_to_string(&template_local_path).await?;
    contents = contents.replace("{{property address}}", &metadata.full_address());
    contents = contents.replace("{{url}}", &format!("[URL]({})", metadata.url));
    contents = contents.replace(
        "{{price}}",
        &format!("${}", metadata.price.to_formatted_string(&Locale::en)),
    );
    contents = contents.replace("{{description}}", &metadata.description);
    contents = contents.replace("{{mls}}", &metadata.mls);
    contents = contents.replace("{{photo}}", "![First image](images/compass/1.webp)");

    fs::write(&template_local_path, contents).await?;

    let remote_info_path = format!("{remote_property_dir}/info.md");

    let tx_clone = tx.clone();
    nc_client
        .upload(&template_local_path, &remote_info_path, tx_clone)
        .await?;
    println!("Property info template updated and uploaded to {remote_info_path}");
    let _ = tx
        .send(format!(
            "Property info template updated and uploaded to {remote_info_path}"
        ))
        .await;

    Ok(())
}

#[derive(Deserialize)]
struct FormData {
    listing_text: String,
}

async fn index() -> impl Responder {
    HttpResponse::Ok()
        .content_type("text/html; charset=utf-8")
        .body(include_str!("../static/index.html"))
}

async fn handle_form_submission(form: web::Form<FormData>) -> HttpResponse {
    let (tx, rx) = mpsc::channel::<String>(10);

    let url = form.listing_text.trim().to_string();
    tokio::spawn(async move {
        if url.is_empty() {
            let _ = tx.send("Listing URL cannot be empty.\n".to_string()).await;
            return;
        }

        if (!url.starts_with("http://") && !url.starts_with("https://"))
            || !url.contains("compass.com/listing/")
        {
            let _ = tx.send("Invalid Compass listing URL.\n".to_string()).await;
            return;
        }

        let tx_clone = tx.clone();
        match create_listing(url, tx_clone).await {
            Ok(()) => {
                let _ = tx.send("Listing created successfully.\n".to_string()).await;
            }
            Err(e) => {
                let _ = tx.send(format!("Failed to create listing: {e}\n")).await;
            }
        }
    });

    let stream = ReceiverStream::new(rx)
        .map(|s| Ok(web::Bytes::from(s)) as Result<web::Bytes, actix_web::Error>);

    HttpResponse::Ok()
        .content_type("text/plain; charset=utf-8")
        .streaming(stream)
}

#[tokio::main]
async fn main() -> Result<(), AppError> {
    println!("Server starting at http://0.0.0.0:8080");

    HttpServer::new(|| {
        App::new()
            .route("/", web::get().to(index))
            .route("/create", web::post().to(handle_form_submission))
    })
    .bind(("0.0.0.0", 8080))? //TODO: Make port configurable
    .run()
    .await?;

    Ok(())
}
