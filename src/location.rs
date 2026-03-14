//! Location awareness via IP geolocation.
//!
//! Fetches approximate location from ipinfo.io and caches it for one hour.
//! Used by location-aware profiles (e.g., daily) to inject geographic context.

use std::sync::Arc;
use std::time::{Duration, Instant};

use arc_swap::ArcSwapOption;
use zenwave::ResponseExt;

const IPINFO_URL: &str = "https://ipinfo.io/json";
const CACHE_TTL: Duration = Duration::from_secs(3600); // 1 hour

/// Approximate geographic location derived from IP.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LocationInfo {
    pub city: String,
    pub region: String,
    pub country: String,
    pub timezone: String,
}

#[derive(Debug)]
struct CachedLocation {
    fetched_at: Instant,
    info: LocationInfo,
}

/// Cached location provider with automatic refresh.
pub struct LocationProvider {
    cache: ArcSwapOption<CachedLocation>,
}

impl LocationProvider {
    pub fn new() -> Self {
        Self {
            cache: ArcSwapOption::empty(),
        }
    }

    /// Get the current location, fetching from ipinfo.io if cache is stale.
    pub async fn get(&self) -> anyhow::Result<LocationInfo> {
        if let Some(cached) = self.cache.load_full()
            && cached.fetched_at.elapsed() < CACHE_TTL
        {
            return Ok(cached.info.clone());
        }

        let info = Self::fetch().await?;
        self.cache.store(Some(Arc::new(CachedLocation {
            fetched_at: Instant::now(),
            info: info.clone(),
        })));
        Ok(info)
    }

    /// Fetch location from ipinfo.io.
    async fn fetch() -> anyhow::Result<LocationInfo> {
        let response = zenwave::get(IPINFO_URL).await?;
        let info: LocationInfo = response.into_json().await?;
        tracing::debug!(
            city = %info.city,
            region = %info.region,
            country = %info.country,
            timezone = %info.timezone,
            "Fetched location from ipinfo.io"
        );
        Ok(info)
    }
}
