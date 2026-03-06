//! Location awareness via IP geolocation.
//!
//! Fetches approximate location from ipinfo.io and caches it for one hour.
//! Used by location-aware profiles (e.g., daily) to inject geographic context.

use std::time::{Duration, Instant};

use tokio::sync::RwLock;
use zenwave::ResponseExt;

const IPINFO_URL: &str = "https://ipinfo.io/json";
const CACHE_TTL: Duration = Duration::from_secs(3600); // 1 hour
const LOCATION_TEMPLATE: &str = include_str!("../templates/location_context.xml");

/// Approximate geographic location derived from IP.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LocationInfo {
    pub city: String,
    pub region: String,
    pub country: String,
    pub timezone: String,
}

impl LocationInfo {
    /// Format as XML context string for agent injection.
    pub fn to_context_string(&self) -> String {
        LOCATION_TEMPLATE
            .replace("{{ city }}", &self.city)
            .replace("{{ region }}", &self.region)
            .replace("{{ country }}", &self.country)
            .replace("{{ timezone }}", &self.timezone)
    }
}

/// Cached location provider with automatic refresh.
pub struct LocationProvider {
    cache: RwLock<Option<(Instant, LocationInfo)>>,
}

impl LocationProvider {
    pub fn new() -> Self {
        Self {
            cache: RwLock::new(None),
        }
    }

    /// Get the current location, fetching from ipinfo.io if cache is stale.
    pub async fn get(&self) -> anyhow::Result<LocationInfo> {
        // Fast path: return cached if fresh
        {
            let cached = self.cache.read().await;
            if let Some((fetched_at, ref info)) = *cached {
                if fetched_at.elapsed() < CACHE_TTL {
                    return Ok(info.clone());
                }
            }
        }

        let info = Self::fetch().await?;
        let mut cache = self.cache.write().await;
        *cache = Some((Instant::now(), info.clone()));
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
