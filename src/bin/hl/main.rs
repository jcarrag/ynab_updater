#![feature(iterator_try_collect)]

use anyhow::Result;
use regex::Regex;
use scraper::{Html, Selector};
use serde::Deserialize;
use std::env;
use ynab_updater::{
    CONFIG_FILENAME, GetBalance, GetYnabAccountConfig, YnabAccountConfig, update_ynab,
};

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub struct Config {
    pub hl_username: String,
    pub hl_date_of_birth: String,
    pub hl_password: String,
    pub hl_secure_numbers: [String; 6],

    pub ynab_hl_account_id: String,
}

struct Mock {}

struct HL {}

impl GetYnabAccountConfig for Mock {
    async fn get(&self) -> Result<YnabAccountConfig> {
        get_hl_ynab_account_config()
    }
}

impl GetBalance for Mock {
    async fn get(&self) -> Result<f32> {
        Ok(0.0)
    }
}

impl GetYnabAccountConfig for HL {
    async fn get(&self) -> Result<YnabAccountConfig> {
        get_hl_ynab_account_config()
    }
}

impl GetBalance for HL {
    async fn get(&self) -> Result<f32> {
        let config_path = format!("{}/{}", env::var("YNAB_CONFIG_PATH")?, CONFIG_FILENAME);

        let config = config::Config::builder()
            .add_source(config::File::with_name(&config_path))
            .build()?
            .try_deserialize::<Config>()?;

        let client = reqwest::Client::builder().cookie_store(true).build()?;

        let hl_vt = get_hl_vt(&client).await?;

        login_step_one(&config, &client, hl_vt.as_str()).await?;

        let secure_number_indices = login_step_two(&client).await?;

        let home_page =
            submit_secure_number(&config, &client, hl_vt, secure_number_indices).await?;

        let hl_balance = get_total(home_page).await?;

        Ok(hl_balance)
    }
}

fn get_hl_ynab_account_config() -> Result<YnabAccountConfig> {
    let config_path = format!("{}/{}", env::var("YNAB_CONFIG_PATH")?, CONFIG_FILENAME);

    let config = config::Config::builder()
        .add_source(config::File::with_name(&config_path))
        .build()?
        .try_deserialize::<Config>()?;

    let yac = YnabAccountConfig {
        ynab_account_id: config.ynab_hl_account_id,
    };

    Ok(yac)
}

async fn get_hl_vt(client: &reqwest::Client) -> Result<String> {
    let resp = client
        .get("https://online.hl.co.uk/my-accounts/login-step-one")
        .send()
        .await?;
    let text = resp.text().await?;
    let document = Html::parse_fragment(&text);
    let selector_string = r#"input[name="hl_vt"]"#;
    let selector = Selector::parse(selector_string).unwrap();
    let hl_vt = document
        .select(&selector)
        .next()
        .ok_or(format!("Failed to match selector: {}", selector_string))
        .unwrap()
        .value()
        .attr("value")
        .ok_or("Failed to get 'value' from selected node")
        .unwrap()
        .to_owned();

    Ok(hl_vt)
}

async fn login_step_one(
    config: &Config,
    client: &reqwest::Client,
    hl_vt: &str,
) -> Result<(), reqwest::Error> {
    let params = [
        ("hl_vt", hl_vt),
        ("username", config.hl_username.as_str()),
        ("date-of-birth", config.hl_date_of_birth.as_str()),
    ];
    client
        .post("https://online.hl.co.uk/my-accounts/login-step-one")
        .form(&params)
        .send()
        .await?;
    Ok(())
}

async fn login_step_two(client: &reqwest::Client) -> Result<Vec<usize>> {
    let resp = client
        .get("https://online.hl.co.uk/my-accounts/login-step-two")
        .send()
        .await?;
    let text = resp.text().await?;
    let document = Html::parse_fragment(&text);

    let regex = Regex::new(r"Enter the (\d)\w{2} digit from your Secure Number")?;

    let titles = (1..=3)
        .map(|i| -> Result<usize> {
            let selector_string = format!(r#"input[id="secure-number-{}"]"#, i);
            let selector = Selector::parse(&selector_string)
                .map_err(|_| format!("Failed to parse selector: {:#?}", selector_string))
                .unwrap();
            let title = document
                .clone()
                .select(&selector)
                .next()
                .ok_or(format!("Failed to match selector: {}", selector_string))
                .unwrap()
                .value()
                .attr("title")
                .ok_or("Failed to get 'title' from selected node")
                .unwrap()
                .to_owned();

            let digit_match = regex
                .captures(title.as_str())
                .ok_or("")
                .unwrap()
                .get(1)
                .ok_or("")
                .unwrap()
                .as_str();
            Ok(digit_match.parse::<usize>()? - 1)
        })
        .try_collect::<Vec<_>>();

    titles
}

async fn submit_secure_number(
    config: &Config,
    client: &reqwest::Client,
    hl_vt: String,
    secure_number_indices: Vec<usize>,
) -> Result<String, reqwest::Error> {
    let params = [
        ("hl_vt", hl_vt.as_str()),
        ("online-password-verification", config.hl_password.as_str()),
        (
            "secure-number[1]",
            config.hl_secure_numbers[secure_number_indices[0]].as_str(),
        ),
        (
            "secure-number[2]",
            config.hl_secure_numbers[secure_number_indices[1]].as_str(),
        ),
        (
            "secure-number[3]",
            config.hl_secure_numbers[secure_number_indices[2]].as_str(),
        ),
        ("submit", " Log in   "),
    ];

    let resp = client
        .post("https://online.hl.co.uk/my-accounts/login-step-two")
        .form(&params)
        .send()
        .await?;

    let text = resp.text().await?;

    Ok(text)
}

async fn get_total(home_page: String) -> Result<f32> {
    let document = Html::parse_fragment(&home_page);

    let total = (2..=3).map(|i| {
        let selector_string = format!(r#"#content-body-full > div > div.main-content > table > tfoot > tr > td:nth-child({})"#, i);
        let selector = Selector::parse(&selector_string).map_err(|_| format!("Failed to parse selector: {:#?}", selector_string)).unwrap();

        let totals = document
            .select(&selector)
            .next()
            .ok_or(format!("Failed to match selector: {}", selector_string))
            .unwrap()
            .text()
            .next()
            .ok_or("Failed to get 'text' from selected node")
            .unwrap()
            .to_owned();

        let regex = Regex::new(r"\W*(\d*\,?\d*\.?\d{2}?)")?;

        let captures = regex
            .captures(&totals)
            .ok_or("Failed to get captures from regex")
            .unwrap()
            .get(1)
            .ok_or("Failed to get match from regex")
            .unwrap()
            .as_str()
            .replace(',', "");

        Ok(captures.parse::<f32>()?)
    }).sum::<Result<f32, _>>();

    total
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::init();

    let _hl = HL {};

    let _mock = Mock {};

    update_ynab(_hl).await
}
