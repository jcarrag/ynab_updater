#![feature(iterator_try_collect)]

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Duration, Utc};
use log::info;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::str::from_utf8;
use std::sync::Arc;
use std::{env, net::TcpListener};
use ynab_updater::{
    CONFIG_FILENAME, GetBalance, GetYnabAccountConfig, YnabAccountConfig,
    pushover::{self, SendMessage},
    update_ynab,
};

static SAXO_AUTH_URL: &str = "https://live.logonvalidation.net/authorize";
static SAXO_ACCESS_URL: &str = "https://live.logonvalidation.net/token";
static SAXO_API_URL: &str = "https://gateway.saxobank.com/openapi/";

static ACCESS_TOKEN_FILENAME: &str = "access_token.json";
static TLS_CERT_FILENAME: &str = "tailscale.crt";
static TLS_KEY_FILENAME: &str = "tailscale.key";

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub struct Config {
    // Where this binary caches its own runtime state (access_token.json, the
    // Tailscale TLS cert/key) - deliberately distinct from YNAB_CONFIG_PATH
    // (which locates settings.toml itself), since on the deployed service
    // that's a read-only agenix-managed directory.
    #[serde(rename = "state_path")]
    pub state_path: String,
    #[serde(rename = "tailscale_ip")]
    pub tailscale_ip: String,

    pub saxo_client_id: String,
    pub saxo_client_secret: String,
    pub saxo_redirect_uri: String,

    pub ynab_saxo_account_id: String,

    pub pushover_user_key: String,
    pub pushover_api_key: String,
}

struct Mock {}

struct Saxo {}

impl GetYnabAccountConfig for Mock {
    async fn get(&self) -> Result<YnabAccountConfig> {
        get_saxo_ynab_account_config()
    }
}

impl GetBalance for Mock {
    async fn get(&self) -> Result<f32> {
        Ok(0.0)
    }
}

impl GetYnabAccountConfig for Saxo {
    async fn get(&self) -> Result<YnabAccountConfig> {
        get_saxo_ynab_account_config()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct AccessTokenResponse {
    access_token: String,
    expires_in: u32,
    refresh_token: String,
    refresh_token_expires_in: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct AccountResponse {
    total_value: f32,
}

impl GetBalance for Saxo {
    async fn get(&self) -> Result<f32> {
        let config_path = format!("{}/{}", env::var("YNAB_CONFIG_PATH")?, CONFIG_FILENAME);

        let config = config::Config::builder()
            .add_source(config::File::with_name(&config_path))
            .add_source(config::Environment::with_prefix("YNAB"))
            .build()?
            .try_deserialize::<Config>()?;

        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()?;

        let api = pushover::API::new();

        let refreshed_access_token = get_refreshed_access_token(&config, &client, &api).await?;

        let account_response = get_account_value(&client, &refreshed_access_token).await?;

        Ok(account_response.total_value)
    }
}

async fn get_refreshed_access_token(
    config: &Config,
    client: &reqwest::Client,
    api: &pushover::API,
) -> Result<AccessTokenResponse> {
    let access_token = get_cached_or_live_access_token(config, client, api).await?;

    let refreshed_access_token = refresh_access_token(config, client, &access_token).await?;

    std::fs::write(
        get_access_token_path(config),
        serde_json::to_string(&refreshed_access_token)?,
    )?;

    Ok(refreshed_access_token)
}

fn get_access_token_path(config: &Config) -> String {
    format!("{}/{}", config.state_path, ACCESS_TOKEN_FILENAME)
}

async fn get_cached_or_live_access_token(
    config: &Config,
    client: &reqwest::Client,
    api: &pushover::API,
) -> Result<AccessTokenResponse> {
    let access_token_path = get_access_token_path(config);

    let valid_refresh_token_o = std::fs::metadata(access_token_path.clone())
        .ok()
        .and_then(|stat| stat.modified().ok())
        .and_then(|modified| {
            let access_token_file = std::fs::read(access_token_path.clone())
                .unwrap_or_else(|_| panic!("Unable to read {}", access_token_path));

            let access_token = serde_json::from_slice::<AccessTokenResponse>(&access_token_file)
                .expect("Unable to parse access_token_file");

            let modified_at = DateTime::<Utc>::from(modified);

            let expires_in = Duration::seconds(access_token.refresh_token_expires_in as i64);

            let expires_at = modified_at
                .checked_add_signed(expires_in)
                .unwrap_or_else(|| {
                    panic!(
                        "Unable to add expires_in '{}' to modified_at '{}'",
                        expires_in, modified_at
                    )
                });

            if Utc::now() > expires_at {
                None
            } else {
                Some(access_token)
            }
        });

    match valid_refresh_token_o {
        Some(valid_refresh_token) => Ok(valid_refresh_token),
        _ => {
            let login_uri = get_login_uri(config, client).await?;

            send_login_uri_push_notification(config, api, login_uri).await?;

            let auth_code = block_until_auth_code(config)?;

            let access_token = get_access_token(config, client, auth_code).await?;

            std::fs::write(access_token_path, serde_json::to_string(&access_token)?)?;

            Ok(access_token)
        }
    }
}

fn get_saxo_ynab_account_config() -> Result<YnabAccountConfig> {
    let config_path = format!("{}/{}", env::var("YNAB_CONFIG_PATH")?, CONFIG_FILENAME);

    let config = config::Config::builder()
        .add_source(config::File::with_name(&config_path))
        .add_source(config::Environment::with_prefix("YNAB"))
        .build()?
        .try_deserialize::<Config>()?;

    let yac = YnabAccountConfig {
        ynab_account_id: config.ynab_saxo_account_id,
    };

    Ok(yac)
}

async fn get_login_uri(config: &Config, client: &reqwest::Client) -> Result<String> {
    let response = client
        .get(SAXO_AUTH_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .query(&[
            ("response_type", "code"),
            ("client_id", config.saxo_client_id.as_str()),
            ("state", "0"),
            ("redirect_uri", config.saxo_redirect_uri.as_str()),
        ])
        .send()
        .await?;

    let status = response.status();

    // expires on 5/11/2031 - renew at https://www.developer.saxo/openapi/appmanagement#/livedetails/$CLIENT_ID
    let location = match response.headers().get("location") {
        Some(location) => location.to_str()?.to_owned(),
        None => {
            let body = response
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read response body: {e}>"));

            bail!(
                "Unable to get Location header from Saxo auth response (status {status}): {body}"
            );
        }
    };

    Ok(location)
}

fn tls_cert_path(config: &Config) -> String {
    format!("{}/{}", config.state_path, TLS_CERT_FILENAME)
}

fn tls_key_path(config: &Config) -> String {
    format!("{}/{}", config.state_path, TLS_KEY_FILENAME)
}

fn load_tls_server_config(config: &Config) -> Result<rustls::ServerConfig> {
    let cert_file =
        std::fs::File::open(tls_cert_path(config)).context("Unable to open TLS cert file")?;
    let certs = rustls_pemfile::certs(&mut std::io::BufReader::new(cert_file))
        .try_collect::<Vec<_>>()
        .context("Unable to parse TLS cert file")?;

    let key_file =
        std::fs::File::open(tls_key_path(config)).context("Unable to open TLS key file")?;
    let key = rustls_pemfile::private_key(&mut std::io::BufReader::new(key_file))
        .context("Unable to parse TLS key file")?
        .context("No private key found in TLS key file")?;

    let server_config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .context("Unable to build TLS server config")?;

    Ok(server_config)
}

fn block_until_auth_code(config: &Config) -> Result<String> {
    info!("Waiting for auth code redirect");

    let tls_config = Arc::new(load_tls_server_config(config)?);

    let listener = TcpListener::bind(format!("{}:9999", config.tailscale_ip))?;

    let (mut tcp_stream, _) = listener.accept()?;

    let mut conn = rustls::ServerConnection::new(tls_config)?;
    let mut stream = rustls::Stream::new(&mut conn, &mut tcp_stream);

    let mut buffer = [0; 512];
    stream.read_exact(&mut buffer).unwrap();

    info!(
        "buffer size: {:?}, str: {:?}, content: {:?}",
        buffer.len(),
        from_utf8(&buffer),
        buffer.clone().to_ascii_uppercase()
    );

    stream.write_all("HTTP/1.1 200 OK\r\nContent-Length: 7\r\n\r\nsuccess".as_bytes())?;
    stream.flush()?;

    let mut headers = [httparse::EMPTY_HEADER; 20];
    let mut req = httparse::Request::new(&mut headers);
    info!("pres req content: {:?}", req);
    req.parse(&buffer)?;
    info!("parsed req content: {:?}", req);

    let req = reqwest::Url::parse(format!("http://_{}", req.path.unwrap()).as_str())?;
    info!("2 parsed req content: {:?}", req);

    let code = req
        .query_pairs()
        .find(|s| s.0 == "code")
        .expect("Unable to parse code from redirect_uri")
        .1
        .into_owned();

    info!("2 req code: {:?}", code);

    Ok(code)
}

async fn send_login_uri_push_notification(
    config: &Config,
    api: &pushover::API,
    login_uri: String,
) -> Result<()> {
    let mut msg = SendMessage::new(
        config.pushover_api_key.clone(),
        config.pushover_user_key.clone(),
        "Login to Saxo",
    );
    msg.set_url(login_uri.clone());
    msg.set_url_title("Login link");

    api.send(&msg).await.unwrap();

    Ok(())
}

async fn get_access_token(
    config: &Config,
    client: &reqwest::Client,
    code: String,
) -> Result<AccessTokenResponse> {
    let params = HashMap::from([
        ("client_id", config.saxo_client_id.as_str()),
        ("client_secret", config.saxo_client_secret.as_str()),
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", config.saxo_redirect_uri.as_str()),
    ]);

    let token = client
        .post(SAXO_ACCESS_URL)
        .form(&params)
        .send()
        .await?
        .json::<AccessTokenResponse>()
        .await?;

    Ok(token)
}

async fn refresh_access_token(
    config: &Config,
    client: &reqwest::Client,
    access_token: &AccessTokenResponse,
) -> Result<AccessTokenResponse> {
    let params = HashMap::from([
        ("client_id", config.saxo_client_id.as_str()),
        ("client_secret", config.saxo_client_secret.as_str()),
        ("grant_type", "refresh_token"),
        ("refresh_token", access_token.refresh_token.as_str()),
        ("redirect_uri", config.saxo_redirect_uri.as_str()),
    ]);

    let token = client
        .post(SAXO_ACCESS_URL)
        .form(&params)
        .send()
        .await?
        .json::<AccessTokenResponse>()
        .await?;

    Ok(token)
}

async fn get_account_value(
    client: &reqwest::Client,
    access_token: &AccessTokenResponse,
) -> Result<AccountResponse> {
    let resp = client
        .get(format!("{}/port/v1/balances/me", SAXO_API_URL))
        .bearer_auth(access_token.access_token.clone())
        .send()
        .await?
        .json::<AccountResponse>()
        .await?;

    Ok(resp)
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("Unable to install rustls default crypto provider");

    let _saxo = Saxo {};

    let _mock = Mock {};

    update_ynab(_saxo).await
}
