//! The management-plane weather poller.
//!
//! One tokio task (never an engine thread) owns every product fetch. It is
//! idle while no location is configured. Each **distinct** product id is
//! fetched at most once per [`MIN_INTERVAL`] (10 min), failures back off
//! exponentially (capped at [`MAX_BACKOFF`]), and every schedule carries a
//! small jitter so many instances do not hit BOM in lock-step. The last good
//! parse is kept across failures; freshness (`fetched_at`, `attempted_at`,
//! `error`) is tracked per product. After every pass the task publishes the
//! normalized per-location view into the shared [`WeatherStore`], which the
//! render workers read without blocking.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::Utc;
use tokio::sync::watch;
use tokio::time::Instant;

use crate::model::{
    normalize_forecast, normalize_observation, Freshness, LocationWeather, WeatherStore,
};
use crate::{Forecast, Observations};

/// Minimum spacing between two fetches of the same product.
pub const MIN_INTERVAL: Duration = Duration::from_secs(10 * 60);
/// Upper bound on the failure backoff.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60 * 60);
/// Maximum jitter added to every schedule.
pub const MAX_JITTER: Duration = Duration::from_secs(60);

/// One configured location, as the poller needs it (decoupled from the config
/// crate so this crate stays a leaf).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LocationSpec {
    /// Location id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// Forecast product id.
    pub forecast_product: String,
    /// Forecast area code.
    pub aac: String,
    /// Observation product id + station WMO id.
    pub observation: Option<(String, String)>,
}

/// Fetches one product's raw XML (FTP in production, scripted in tests).
pub trait Fetcher: Send + Sync + 'static {
    /// Fetch `product` (e.g. `IDN11060`).
    fn fetch(&self, product: &str) -> impl Future<Output = Result<Vec<u8>, String>> + Send;
}

/// The production fetcher: anonymous passive FTP from `ftp.bom.gov.au`.
#[derive(Clone, Debug)]
pub struct BomFtpFetcher {
    /// Server host name.
    pub host: String,
    /// Server port.
    pub port: u16,
    /// Transfer bounds.
    pub limits: crate::ftp::FtpLimits,
}

impl Default for BomFtpFetcher {
    fn default() -> Self {
        Self {
            host: "ftp.bom.gov.au".to_owned(),
            port: 21,
            limits: crate::ftp::FtpLimits::default(),
        }
    }
}

impl Fetcher for BomFtpFetcher {
    async fn fetch(&self, product: &str) -> Result<Vec<u8>, String> {
        crate::ftp::retr(
            &self.host,
            self.port,
            &crate::ftp::bom_product_path(product),
            self.limits,
        )
        .await
        .map_err(|e| e.to_string())
    }
}

#[derive(Clone, Debug)]
enum Parsed {
    Forecast(Arc<Forecast>),
    Observations(Arc<Observations>),
}

#[derive(Debug)]
struct ProductState {
    last_good: Option<Parsed>,
    freshness: Freshness,
    next_due: Instant,
    failures: u32,
}

/// The cloneable handle the control plane holds: location updates in,
/// freshness/status out. Cheap to clone.
#[derive(Clone, Debug)]
pub struct WeatherService {
    store: Arc<WeatherStore>,
    locations: Arc<watch::Sender<Vec<LocationSpec>>>,
    products: Arc<Mutex<BTreeMap<String, Freshness>>>,
}

impl Default for WeatherService {
    fn default() -> Self {
        Self::new(Arc::new(WeatherStore::default()))
    }
}

impl WeatherService {
    /// A handle publishing into `store`. No task runs until [`Self::spawn`].
    #[must_use]
    pub fn new(store: Arc<WeatherStore>) -> Self {
        let (tx, _rx) = watch::channel(Vec::new());
        Self {
            store,
            locations: Arc::new(tx),
            products: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// The store this service publishes into.
    #[must_use]
    pub fn store(&self) -> &Arc<WeatherStore> {
        &self.store
    }

    /// Replace the configured locations (wakes the poller; publishes an
    /// immediate placeholder view so a new location is visible at once).
    pub fn set_locations(&self, locations: Vec<LocationSpec>) {
        self.locations.send_replace(locations);
        self.republish(&BTreeMap::new());
    }

    /// The configured locations.
    #[must_use]
    pub fn locations(&self) -> Vec<LocationSpec> {
        self.locations.borrow().clone()
    }

    /// Freshness of one product (`None` when never attempted).
    #[must_use]
    pub fn product_freshness(&self, product: &str) -> Option<Freshness> {
        self.products.lock().ok()?.get(product).cloned()
    }

    /// Re-publish the per-location view. `parsed` holds the poller's current
    /// last-good parses; an empty map means "keep whatever is published"
    /// (used for config edits between polls).
    fn republish(&self, parsed: &BTreeMap<String, Parsed>) {
        let freshness = self.products.lock().map(|p| p.clone()).unwrap_or_default();
        let locations = self.locations.borrow().clone();
        let previous: BTreeMap<String, Arc<LocationWeather>> = self
            .store
            .all()
            .into_iter()
            .map(|p| (p.data.location_id.clone(), p.data))
            .collect();
        let mut out = Vec::with_capacity(locations.len());
        for loc in &locations {
            let prev = previous.get(&loc.id);
            let forecast = match parsed.get(&loc.forecast_product) {
                Some(Parsed::Forecast(f)) => normalize_forecast(f, &loc.aac),
                _ => prev
                    .and_then(|p| p.forecast.clone())
                    .filter(|f| f.product == loc.forecast_product && f.aac == loc.aac),
            };
            let observation =
                loc.observation
                    .as_ref()
                    .and_then(|(product, wmo)| match parsed.get(product) {
                        Some(Parsed::Observations(o)) => normalize_observation(o, wmo),
                        _ => prev
                            .and_then(|p| p.observation.clone())
                            .filter(|o| &o.product == product && &o.wmo_id == wmo),
                    });
            out.push(LocationWeather {
                location_id: loc.id.clone(),
                name: loc.name.clone(),
                forecast,
                forecast_freshness: freshness
                    .get(&loc.forecast_product)
                    .cloned()
                    .unwrap_or_default(),
                observation,
                observation_freshness: loc
                    .observation
                    .as_ref()
                    .map(|(p, _)| freshness.get(p).cloned().unwrap_or_default()),
            });
        }
        self.store.publish_all(out);
    }

    /// Spawn the poller task on the current tokio runtime. It exits when the
    /// last handle is dropped or `shutdown` resolves.
    pub fn spawn<F: Fetcher>(
        &self,
        fetcher: F,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> tokio::task::JoinHandle<()> {
        let this = self.clone();
        tokio::spawn(async move {
            tokio::select! {
                () = this.run(fetcher) => {}
                () = shutdown => {}
            }
        })
    }

    async fn run<F: Fetcher>(self, fetcher: F) {
        let mut rx = self.locations.subscribe();
        let mut states: BTreeMap<String, ProductState> = BTreeMap::new();
        loop {
            let wanted = distinct_products(&rx.borrow_and_update());
            states.retain(|p, _| wanted.contains_key(p));
            if let Ok(mut f) = self.products.lock() {
                f.retain(|p, _| wanted.contains_key(p));
            }
            let now = Instant::now();
            for (product, is_forecast) in &wanted {
                let state = states
                    .entry(product.clone())
                    .or_insert_with(|| ProductState {
                        last_good: None,
                        freshness: Freshness::default(),
                        next_due: now,
                        failures: 0,
                    });
                if state.next_due > now {
                    continue;
                }
                let result = fetcher.fetch(product).await;
                let parsed = match result {
                    Ok(bytes) => parse_product(bytes, *is_forecast).await,
                    Err(e) => Err(e),
                };
                let finished = Utc::now();
                state.freshness.attempted_at = Some(finished);
                match parsed {
                    Ok(p) => {
                        state.last_good = Some(p);
                        state.freshness.fetched_at = Some(finished);
                        state.freshness.error = None;
                        state.failures = 0;
                        state.next_due = Instant::now() + MIN_INTERVAL + jitter(product);
                    }
                    Err(e) => {
                        tracing::warn!(product = %product, error = %e, "weather fetch failed; keeping last good");
                        state.freshness.error = Some(e);
                        state.failures = state.failures.saturating_add(1);
                        state.next_due = Instant::now() + backoff(state.failures) + jitter(product);
                    }
                }
                if let Ok(mut f) = self.products.lock() {
                    f.insert(product.clone(), state.freshness.clone());
                }
            }
            let parsed: BTreeMap<String, Parsed> = states
                .iter()
                .filter_map(|(k, s)| s.last_good.clone().map(|p| (k.clone(), p)))
                .collect();
            self.republish(&parsed);
            // Sleep until the earliest due product, or until the location set
            // changes. Idle (no products) waits only on a config change.
            let next = states.values().map(|s| s.next_due).min();
            let changed = async {
                if rx.changed().await.is_err() {
                    // Every sender dropped: park forever (the select in
                    // `spawn` still observes shutdown).
                    std::future::pending::<()>().await;
                }
            };
            match next {
                Some(at) => {
                    tokio::select! {
                        () = tokio::time::sleep_until(at) => {}
                        () = changed => {}
                    }
                }
                None => changed.await,
            }
        }
    }
}

/// Distinct product ids wanted by `locations`, flagged forecast (`true`) or
/// observation (`false`).
fn distinct_products(locations: &[LocationSpec]) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    for l in locations {
        out.insert(l.forecast_product.clone(), true);
        if let Some((p, _)) = &l.observation {
            out.entry(p.clone()).or_insert(false);
        }
    }
    out
}

async fn parse_product(bytes: Vec<u8>, is_forecast: bool) -> Result<Parsed, String> {
    // XML parsing is CPU work: keep it off the async workers.
    tokio::task::spawn_blocking(move || {
        let limits = crate::Limits {
            max_bytes: crate::ftp::MAX_PRODUCT_BYTES,
            max_nodes: 400_000,
            ..crate::Limits::default()
        };
        if is_forecast {
            crate::parse_forecast_with_limits(&bytes, limits)
                .map(|f| Parsed::Forecast(Arc::new(f)))
                .map_err(|e| format!("parse: {e}"))
        } else {
            crate::parse_observations_with_limits(&bytes, limits)
                .map(|o| Parsed::Observations(Arc::new(o)))
                .map_err(|e| format!("parse: {e}"))
        }
    })
    .await
    .map_err(|e| format!("parse task: {e}"))?
}

/// Failure backoff: 10 min, 20 min, 40 min, then capped at 60 min.
#[must_use]
pub fn backoff(failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(8);
    MIN_INTERVAL.saturating_mul(1_u32 << shift).min(MAX_BACKOFF)
}

/// Deterministic-per-call jitter in `0..MAX_JITTER` without an RNG dependency.
fn jitter(product: &str) -> Duration {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    product.hash(&mut h);
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos())
        .hash(&mut h);
    let max_ms = u64::try_from(MAX_JITTER.as_millis())
        .unwrap_or(60_000)
        .max(1);
    Duration::from_millis(h.finish() % max_ms)
}

/// Every product id referenced by `locations` (for diagnostics).
#[must_use]
pub fn products_of(locations: &[LocationSpec]) -> BTreeSet<String> {
    distinct_products(locations).into_keys().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const FORECAST: &str = r#"<product version="1.7"><amoc><identifier>IDN11060</identifier><issue-time-utc>2026-01-01T00:00:00Z</issue-time-utc><expiry-time>2026-01-02T00:00:00Z</expiry-time></amoc><forecast><area aac="NSW_PT131" description="Sydney" type="location"><forecast-period index="0" start-time-local="2026-01-01T10:00:00+11:00" end-time-local="2026-01-02T00:00:00+11:00" start-time-utc="2025-12-31T23:00:00Z" end-time-utc="2026-01-01T13:00:00Z"><element type="forecast_icon_code">1</element><element type="air_temperature_maximum" units="Celsius">25</element><text type="precis">Sunny.</text><text type="probability_of_precipitation">0%</text></forecast-period></area></forecast></product>"#;
    const OBS: &str = r#"<product version="v1.7.1"><amoc><identifier>IDN60920</identifier><issue-time-utc>2026-01-01T00:00:00Z</issue-time-utc></amoc><observations><station bom-id="066214" wmo-id="94768" tz="Australia/Sydney" stn-name="SYDNEY (OBSERVATORY HILL)"><period index="0" time-utc="2026-01-01T00:00:00Z" time-local="2026-01-01T11:00:00+11:00"><level index="0" type="surface"><element type="air_temperature" units="Celsius">22.6</element><element type="rel-humidity" units="%">56</element><element type="wind_dir">ENE</element><element type="wind_spd_kmh" units="km/h">13</element></level></period></station></observations></product>"#;

    #[derive(Clone, Default)]
    struct Fake {
        calls: Arc<AtomicUsize>,
        fail: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Fetcher for Fake {
        #[allow(clippy::unused_async_trait_impl)]
        async fn fetch(&self, product: &str) -> Result<Vec<u8>, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.fail.load(Ordering::SeqCst) {
                return Err("boom".into());
            }
            Ok(match product {
                "IDN11060" => FORECAST.as_bytes().to_vec(),
                "IDN60920" => OBS.as_bytes().to_vec(),
                _ => return Err("unknown".into()),
            })
        }
    }

    fn sydney() -> LocationSpec {
        LocationSpec {
            id: "sydney".into(),
            name: "Sydney".into(),
            forecast_product: "IDN11060".into(),
            aac: "NSW_PT131".into(),
            observation: Some(("IDN60920".into(), "94768".into())),
        }
    }

    async fn settle() {
        for _ in 0..50 {
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn idle_without_locations_then_fetches_each_product_once() {
        let fake = Fake::default();
        let svc = WeatherService::default();
        let _task = svc.spawn(fake.clone(), std::future::pending());
        settle().await;
        assert_eq!(
            fake.calls.load(Ordering::SeqCst),
            0,
            "no locations, no fetch"
        );

        // Two locations sharing products ⇒ still one fetch per product.
        let mut other = sydney();
        other.id = "sydney-2".into();
        svc.set_locations(vec![sydney(), other]);
        for _ in 0..200 {
            if svc
                .store()
                .get("sydney")
                .and_then(|p| p.data.observation.clone())
                .is_some()
            {
                break;
            }
            settle().await;
        }
        assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
        let data = svc.store().get("sydney").unwrap().data;
        let f = data.forecast.as_ref().unwrap();
        assert_eq!(f.days.len(), 1);
        assert_eq!(f.days[0].max_c, Some(25.0));
        assert_eq!(f.days[0].min_c, None, "absent stays absent");
        assert_eq!(data.observation.as_ref().unwrap().air_temp_c, Some(22.6));
        assert!(data.forecast_freshness.fetched_at.is_some());
        // A config change within the interval does not refetch.
        svc.set_locations(vec![sydney()]);
        settle().await;
        assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failure_records_error_and_keeps_placeholder() {
        let fake = Fake::default();
        fake.fail.store(true, Ordering::SeqCst);
        let svc = WeatherService::default();
        let _task = svc.spawn(fake.clone(), std::future::pending());
        svc.set_locations(vec![sydney()]);
        for _ in 0..200 {
            if svc
                .store()
                .get("sydney")
                .is_some_and(|p| p.data.forecast_freshness.error.is_some())
            {
                break;
            }
            settle().await;
        }
        let data = svc.store().get("sydney").unwrap().data;
        assert!(data.forecast.is_none());
        assert_eq!(data.forecast_freshness.error.as_deref(), Some("boom"));
        assert!(svc.product_freshness("IDN11060").unwrap().error.is_some());
    }

    #[test]
    fn backoff_is_bounded_and_never_below_interval() {
        assert_eq!(backoff(1), MIN_INTERVAL);
        assert_eq!(backoff(2), MIN_INTERVAL * 2);
        assert_eq!(backoff(30), MAX_BACKOFF);
        assert!(jitter("IDN11060") < MAX_JITTER);
    }
}
