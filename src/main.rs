use std::{
    collections::HashMap,
    fs::{File, OpenOptions},
    io::{BufRead, Read, Seek, Write},
    thread,
    time::{self, Duration},
};

use chrono::{DateTime, Days, NaiveDate, Utc};
use reqwest::blocking::{Client, ClientBuilder};
use reqwest_retry::{RetryTransientMiddleware, policies::ExponentialBackoff};
use serde::{Deserialize, Deserializer};

const PRODUCTS_URL: &str = "https://tool.orlen.pl/api/wholesalefuelprices/Products";
const PRODUCT_PRICES_URL: &str = "https://tool.orlen.pl/api/wholesalefuelprices/ByProduct";

fn main() {
    tracing_subscriber::fmt().init();
    let retry = reqwest::retry::for_host("tool.orlen.pl").classify_fn(|req_rep| {
        if req_rep.status() == Some(http::StatusCode::SERVICE_UNAVAILABLE)
            || format!("{:?}", req_rep).contains("ConnectionReset")
        {
            log::warn!("Backing off for 5s...");
            thread::sleep(Duration::from_secs(5));
            req_rep.retryable()
        } else {
            req_rep.success()
        }
    });
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(100))
        .retry(retry)
        .build()
        .unwrap();

    let products_result = get_products(&client);
    if let Err(err) = products_result {
        log::error!("Failed to get products, reason={err}");
        panic!()
    }
    update_dump_file(&client, products_result.unwrap());
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct Product {
    id: i32,
    name: String,
    name_en: String,
    symbol: String,
}

fn get_products(client: &Client) -> Result<Vec<Product>, reqwest::Error> {
    client
        .get(PRODUCTS_URL)
        .send()
        .and_then(|body| body.json::<Vec<Product>>())

    // return Ok(serde_json::from_str(&r#"
    // [{"id":41,"name":"Benzyna bezołowiowa - Eurosuper 95","nameEn":"Unleaded Eurosuper 95","symbol":"Pb95"},{"id":42,"name":"Benzyna bezołowiowa - Super Plus 98","nameEn":"Unleaded Super Plus 98","symbol":"Pb98"},{"id":43,"name":"Olej Napędowy Ekodiesel","nameEn":"Ekodiesel diesel oil","symbol":"ONEkodiesel"},{"id":44,"name":"Olej Napędowy Arktyczny 2","nameEn":"Diesel oil Artic grade A2","symbol":"ONArctic2"},{"id":45,"name":"Olej Napędowy Miejski Super","nameEn":"Super urban diesel oil","symbol":"ONSuper"},{"id":46,"name":"Olej Napędowy Grzewczy Ekoterm","nameEn":"Ekoterm fuel oil","symbol":"OnEkoterm"},{"id":47,"name":"BIO 100","nameEn":"BIO 100","symbol":"BIO100"}]
    // "#).unwrap());
}

#[derive(Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
struct ProductPrice {
    product_name: String,
    #[serde(deserialize_with = "deserialize_utc_datetime")]
    effective_date: NaiveDate,
    #[serde(deserialize_with = "deserialize_utc_datetime")]
    publish_from: NaiveDate,
    value: f32,
    location_name: String,
    location_symbol: String,
    unit: Option<String>,
}

fn deserialize_utc_datetime<'de, D>(deserializer: D) -> Result<NaiveDate, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = String::deserialize(deserializer)?;

    NaiveDate::parse_from_str(&value, "%Y-%m-%dT%H:%M:%S").map_err(serde::de::Error::custom)
}

fn get_prices(client: &Client, product_id: i32, start_from: &NaiveDate) -> Vec<ProductPrice> {
    if start_from >= &Utc::now().date_naive() {
        log::info!("Too early to fetch product prices, product id={}", product_id);
        return vec![];
    }
    let start_from_date = start_from.format("%Y-%m-%d").to_string();
    let url = format!(
        "{}?productId={}&from={}",
        PRODUCT_PRICES_URL, product_id, start_from_date
    );
    log::info!(
        "Fetching product prices, productId = {}, start_from = {}, url = {}",
        product_id,
        start_from_date,
        url
    );
    let mut rows = client
        .get(url)
        .send()
        .and_then(|body| body.json::<Vec<ProductPrice>>())
        .unwrap();

    rows.sort_by(|a,b| a.effective_date.cmp(&b.effective_date));
    rows
}

fn update_dump_file(client: &Client, products: Vec<Product>) {
    let path = format!("prices.csv");
    // let _ = File::create_new(&path);
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .read(true)
        .open(&path)
        .unwrap();
    let mut buff: String = String::new();
    let read_size = file.read_to_string(&mut buff).unwrap();

    let header = "Produkt,Nazwa,Nazwa angielska,Symbol,Data,Opublikowane,Lokalizacja,Symbol Lokalizacji,Cena,Jednostka\n";
    let mut product_last_date_map = HashMap::new();

    if read_size == 0 {
        file.write_all(header.as_bytes()).unwrap();
        log::info!("Wrote CSV header")
    } else {
        log::info!("Reading last metadata");
        let mut first_line_skipped = false;
        for line in buff.lines() {
            log::info!("Line: {}", line);
            if !first_line_skipped {
                first_line_skipped = true;
                continue;
            }
            if line.len() == 0 {
                continue;
            }

            let cols: Vec<String> = line.split(",").map(|s| s.to_owned()).collect();
            let effective_date = NaiveDate::parse_from_str(&cols[5], "%Y-%m-%d").unwrap();
            let product_id: i32 = cols[0].parse::<i32>().unwrap();
            log::info!(
                "Inserting metadata, id={}, v={}",
                product_id,
                effective_date
            );

            product_last_date_map.insert(product_id, effective_date);
        }
    }
    let start_from_date_default = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();

    for product in products {
        let start_from = product_last_date_map
            .get(&product.id)
            .and_then(|date| date.checked_add_days(Days::new(1)))
            .unwrap_or(start_from_date_default);

        let prices = get_prices(client, product.id, &start_from);
        log::info!("Product {} has new {} records", product.id, prices.len());
        for price in prices {
            file.write_all(
                format!(
                    "{},{},{},{},{},{},{},{},{},{}\n",
                    product.id,
                    product.name,
                    product.name_en,
                    product.symbol,
                    price.effective_date,
                    price.publish_from,
                    price.location_name,
                    price.location_symbol,
                    price.value,
                    price.unit.unwrap_or_default()
                )
                .into_bytes()
                .as_slice(),
            )
            .unwrap();
        }
    }

    file.flush().unwrap()
}

#[cfg(test)]
mod deserialization {
    // Note this useful idiom: importing names from outer (for mod tests) scope.
    use super::*;

    #[test]
    fn should_deserialize_product_price() {
        let given = r#"
            
                {
                    "productName": "ONArctic2",
                    "effectiveDate": "2026-10-07T00:00:00",
                    "publishFrom": "2026-10-07T00:00:00",
                    "value": 7339.0,
                    "locationName": "",
                    "locationSymbol": "",
                    "unit": null
                }

        "#;
        let actual = serde_json::from_str::<ProductPrice>(given).unwrap();
    }

    #[test]
    fn should_deserialize_date_to_naive_date() {
        NaiveDate::parse_from_str("2026-10-07", "%Y-%m-%d").unwrap();
    }
}
