//! Chunk bodies are received into memory before the handler runs, while their sender holds a
//! transfer slot: the slots bound how many such bodies are buffered at once, so a stalled sender
//! keeps its slot until the idle or total deadline below ends the request.
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
        // The RV1 uploader retries only `client.TRANSIENT` codes (408 would end its job), and the
        // browser retries any 5xx; nothing was stored, so the retry resumes at the same offset.
        .map_err(|_| ApiError(StatusCode::SERVICE_UNAVAILABLE, "upload_busy"))?;
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
