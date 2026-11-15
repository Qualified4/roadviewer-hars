//! Chunk bodies are received before the handler runs, so a stalled sender cannot hold a transfer slot.
use crate::error::{ApiError, bad};
use axum::{
    body::{Body, Bytes, HttpBody},
    http::StatusCode,
};
use std::{pin::Pin, time::Duration};
use tokio::time::{Instant, timeout_at};

pub struct Limits {
    /// Longest silence between body frames (the device proxy's `client_body_timeout`).
    pub idle: Duration,
    /// Whole-body deadline; the RV1 uploader abandons a chunk request after 120 s.
    pub total: Duration,
    /// The route body limit. Reading stops once it is exceeded and the extractor answers 413 as before.
    pub max: usize,
}

pub async fn receive(mut body: Body, limits: &Limits) -> Result<Bytes, ApiError> {
    let deadline = Instant::now() + limits.total;
    let mut buffer = Vec::new();
    loop {
        let wait = deadline.min(Instant::now() + limits.idle);
        let frame = timeout_at(
            wait,
            std::future::poll_fn(|cx| Pin::new(&mut body).poll_frame(cx)),
        )
        .await
        .map_err(|_| ApiError(StatusCode::REQUEST_TIMEOUT, "request_timeout"))?;
        match frame {
            None => return Ok(buffer.into()),
            Some(Err(_)) => return Err(bad("invalid_chunk")),
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    buffer.extend_from_slice(&data);
                    if buffer.len() > limits.max {
                        return Ok(buffer.into());
                    }
                }
            }
        }
    }
}
