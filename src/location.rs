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

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// Approximate geographic location derived from IP.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct LocationInfo {
    /// City name from the geolocation lookup.
    pub city: String,
    /// Region/state name from the geolocation lookup.
    pub region: String,
    /// Country name or code from the geolocation lookup.
    pub country: String,
    /// IANA timezone name reported for the location.
    pub timezone: String,
}

#[derive(Debug)]
struct CachedLocation {
    fetched_at: Instant,
    info: LocationInfo,
}

/// Cached location provider with automatic refresh.
#[derive(Debug)]
pub struct LocationProvider {
    cache: ArcSwapOption<CachedLocation>,
}

/// Error type for the IP geolocation lookup.
#[derive(Debug, thiserror::Error)]
pub enum LocationError {
    /// Failed to fetch location from ipinfo.io.
    #[error("failed to fetch location from ipinfo.io")]
    Fetch(#[source] BoxError),
    /// Failed to decode location response from ipinfo.io.
    #[error("failed to decode location response from ipinfo.io")]
    Decode(#[source] BoxError),
}

impl Default for LocationProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl LocationProvider {
    /// Create a provider with an empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self {
            cache: ArcSwapOption::empty(),
        }
    }

    /// Get the current location, fetching from ipinfo.io if cache is stale.
    ///
    /// # Errors
    ///
    /// Returns [`LocationError::Fetch`] when the ipinfo.io request fails and
    /// [`LocationError::Decode`] when the response cannot be decoded.
    pub async fn get(&self) -> Result<LocationInfo, LocationError> {
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
    async fn fetch() -> Result<LocationInfo, LocationError> {
        let response = zenwave::get(IPINFO_URL)
            .await
            .map_err(|error| LocationError::Fetch(Box::new(error)))?;
        let info: LocationInfo = response
            .into_json()
            .await
            .map_err(|error| LocationError::Decode(Box::new(error)))?;
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
