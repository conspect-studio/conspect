//! `/api/v1/weather/locations` CRUD + read-back of normalized data fetched by
//! the management-plane poller through a scripted (fake) fetcher.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod support;

use std::time::Duration;

use axum::http::StatusCode;
use multiview_weather::service::{Fetcher, WeatherService};
use serde_json::json;
use support::{
    body_json, delete_if_match, get, harness_with, put_json, send, ADMIN_TOKEN, SCOPED_TOKEN,
    VIEWER_TOKEN,
};

const FORECAST: &str = r#"<product version="1.7"><amoc><identifier>IDN11060</identifier><issue-time-utc>2026-01-01T00:00:00Z</issue-time-utc><expiry-time>2099-01-02T00:00:00Z</expiry-time></amoc><forecast><area aac="NSW_PT131" description="Sydney" type="location"><forecast-period index="0" start-time-local="2026-01-01T10:00:00+11:00" end-time-local="2026-01-02T00:00:00+11:00" start-time-utc="2025-12-31T23:00:00Z" end-time-utc="2026-01-01T13:00:00Z"><element type="forecast_icon_code">1</element><element type="air_temperature_maximum" units="Celsius">25</element><text type="precis">Sunny.</text><text type="probability_of_precipitation">0%</text></forecast-period></area></forecast></product>"#;

#[derive(Clone)]
struct Fake;

impl Fetcher for Fake {
    #[allow(clippy::unused_async_trait_impl)]
    async fn fetch(&self, product: &str) -> Result<Vec<u8>, String> {
        match product {
            "IDN11060" => Ok(FORECAST.as_bytes().to_vec()),
            _ => Err("no such product".into()),
        }
    }
}

fn location(name: &str) -> serde_json::Value {
    json!({ "name": name, "forecast_product": "IDN11060", "aac": "NSW_PT131" })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn weather_location_crud_and_data_read_back() {
    let service = WeatherService::default();
    let _poller = service.spawn(Fake, std::future::pending());
    let h = harness_with(|state| state.with_weather(service.clone()));

    // Empty list.
    let resp = send(&h.router, get("/api/v1/weather/locations", VIEWER_TOKEN)).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await, json!([]));

    // Viewer cannot write; invalid body is 422.
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/weather/locations/sydney",
            VIEWER_TOKEN,
            None,
            &location("Sydney"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/weather/locations/sydney",
            ADMIN_TOKEN,
            None,
            &json!({ "name": "Sydney", "forecast_product": "../etc", "aac": "NSW_PT131" }),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);

    // Create, then replace.
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/weather/locations/sydney",
            ADMIN_TOKEN,
            None,
            &location("Sydney"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::CREATED);
    let resp = send(
        &h.router,
        put_json(
            "/api/v1/weather/locations/sydney",
            ADMIN_TOKEN,
            None,
            &location("Sydney CBD"),
        ),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);

    // The poller fetched through the fake; the normalized day reads back.
    let mut view = serde_json::Value::Null;
    for _ in 0..200 {
        let resp = send(
            &h.router,
            get("/api/v1/weather/locations/sydney", VIEWER_TOKEN),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        view = body_json(resp).await;
        if view["status"] == "ok" {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(view["status"], "ok", "{view}");
    assert_eq!(view["location"]["name"], "Sydney CBD");
    assert_eq!(view["forecast"]["days"][0]["max_c"], 25.0);
    assert_eq!(
        view["forecast"]["days"][0]["min_c"],
        serde_json::Value::Null
    );
    assert_eq!(view["forecast"]["days"][0]["icon_code"], 1);
    assert!(view["forecast_freshness"]["fetched_at"].is_string());
    assert_eq!(view["attribution"], "Source: Bureau of Meteorology");

    // Per-object authz: an out-of-scope principal is 403 on the id and the
    // list hides the row.
    let resp = send(
        &h.router,
        get("/api/v1/weather/locations/sydney", SCOPED_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    let resp = send(&h.router, get("/api/v1/weather/locations", SCOPED_TOKEN)).await;
    assert_eq!(body_json(resp).await, json!([]));

    // Delete; then 404.
    let resp = send(
        &h.router,
        delete_if_match("/api/v1/weather/locations/sydney", ADMIN_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let resp = send(
        &h.router,
        get("/api/v1/weather/locations/sydney", ADMIN_TOKEN),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let resp = send(
        &h.router,
        delete_if_match("/api/v1/weather/locations/sydney", ADMIN_TOKEN, None),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
