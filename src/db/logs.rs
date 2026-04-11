//! Request-log and network-access-log queries on [`ChatDb`].
//!
//! Two tables, four write-and-read methods: `request_logs` captures
//! every LLM request (token accounting, elapsed ms, ok flag) and
//! `network_access_logs` captures every domain decision the sandbox's
//! network policy makes.

use super::{
    ChatDb, ChatDbError, NetworkAccessLogParams, RequestLogParams,
    entity::{network_access_logs, request_logs},
    to_i32, to_i64_from_u64,
};

impl ChatDb {
    pub async fn append_request_log(
        &self,
        params: RequestLogParams<'_>,
    ) -> Result<request_logs::Model, ChatDbError> {
        let prompt_tokens = params
            .prompt_tokens
            .map(|value| to_i32(value, "prompt_tokens"))
            .transpose()?;
        let completion_tokens = params
            .completion_tokens
            .map(|value| to_i32(value, "completion_tokens"))
            .transpose()?;
        let total_tokens = params
            .total_tokens
            .map(|value| to_i32(value, "total_tokens"))
            .transpose()?;
        let cache_read_tokens = params
            .cache_read_tokens
            .map(|value| to_i32(value, "cache_read_tokens"))
            .transpose()?;
        let cache_write_tokens = params
            .cache_write_tokens
            .map(|value| to_i32(value, "cache_write_tokens"))
            .transpose()?;
        let context_window = params
            .context_window
            .map(|value| to_i32(value, "context_window"))
            .transpose()?;
        let elapsed_ms = to_i64_from_u64(params.elapsed_ms, "elapsed_ms")?;

        self.db
            .query(
                "INSERT INTO request_logs (session_id, timestamp, model, provider_id, \
                 prompt_tokens, completion_tokens, total_tokens, cache_read_tokens, \
                 cache_write_tokens, context_window, elapsed_ms, ok, error) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(params.session_id)
            .bind(params.timestamp)
            .bind(params.model)
            .bind(params.provider_id)
            .bind(prompt_tokens)
            .bind(completion_tokens)
            .bind(total_tokens)
            .bind(cache_read_tokens)
            .bind(cache_write_tokens)
            .bind(context_window)
            .bind(elapsed_ms)
            .bind(params.ok)
            .bind(params.error)
            .execute()
            .await?;

        Ok(self
            .db
            .query("SELECT * FROM request_logs ORDER BY timestamp DESC LIMIT ?")
            .bind(1_i64)
            .fetch_one::<request_logs::Model>()
            .await?)
    }

    pub async fn list_request_logs(
        &self,
        limit: u64,
    ) -> Result<Vec<request_logs::Model>, ChatDbError> {
        Ok(self
            .db
            .query("SELECT * FROM request_logs ORDER BY timestamp DESC LIMIT ?")
            .bind(to_i64_from_u64(limit, "request log limit")?)
            .fetch_all::<request_logs::Model>()
            .await?)
    }

    pub async fn append_network_access_log(
        &self,
        params: NetworkAccessLogParams<'_>,
    ) -> Result<network_access_logs::Model, ChatDbError> {
        let timestamp = jiff::Timestamp::now().to_string();
        let port = i32::from(params.port);
        self.db
            .query(
                "INSERT INTO network_access_logs (session_id, profile_id, source, mode, domain, \
                 port, allowed, reason, timestamp) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(params.session_id)
            .bind(params.profile_id)
            .bind(params.source.as_str())
            .bind(params.mode.as_str())
            .bind(params.domain)
            .bind(port)
            .bind(params.allowed)
            .bind(params.reason)
            .bind(timestamp.clone())
            .execute()
            .await?;
        Ok(self
            .db
            .query("SELECT * FROM network_access_logs ORDER BY timestamp DESC LIMIT ?")
            .bind(1_i64)
            .fetch_one::<network_access_logs::Model>()
            .await?)
    }

    pub async fn list_network_access_logs(
        &self,
        limit: u64,
    ) -> Result<Vec<network_access_logs::Model>, ChatDbError> {
        Ok(self
            .db
            .query("SELECT * FROM network_access_logs ORDER BY timestamp DESC LIMIT ?")
            .bind(to_i64_from_u64(limit, "network access log limit")?)
            .fetch_all::<network_access_logs::Model>()
            .await?)
    }
}
