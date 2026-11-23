//! Bounded read-only observation over the existing zbridge v1 transport.
//!
//! The transport owns authentication and the connection. This observer keeps one
//! generation, one lossless cursor and at most one unconsumed event page. It does
//! not discover sessions, reconnect, acknowledge worker claims or send commands.

use std::{future::Future, time::Duration};
use tokio::time::{Instant, sleep_until, timeout_at};
use zbridge::{
    BridgeError, Counter, ErrorCode, EventPage, Id, Operation, Request, Response, ResponseBody,
};

/// A frontend observation window, composed of existing <=1s protocol reads.
pub const MAX_OBSERVATION_MS: u16 = 55_000;
const SINGLE_READ_TIMEOUT: Duration = Duration::from_secs(2);

/// Bind this observer only after capability discovery on an authorized transport.
/// Keep it while a connection is interrupted; reuse it only after rediscovery
/// confirms the exact same generation. A new generation needs a new observer.
pub struct EventObserver {
    generation: Id,
    cursor: Option<Counter>,
    pending: Option<EventPage>,
}

impl EventObserver {
    pub fn new(generation: Id, cursor: Option<Counter>) -> Self {
        Self {
            generation,
            cursor,
            pending: None,
        }
    }

    pub fn generation(&self) -> &Id {
        &self.generation
    }

    /// Cursor safe to resume after an interrupted read. A delivered event page
    /// does not move it until `advance` is called after the frontend applies it.
    pub fn cursor(&self) -> Option<Counter> {
        self.cursor
    }

    pub fn pending_page(&self) -> Option<&EventPage> {
        self.pending.as_ref()
    }

    /// Commit the locally consumed page. After a retention gap, first refresh
    /// current state and reconcile the missing history. This sends no server ack.
    pub fn advance(&mut self) -> Option<Counter> {
        if let Some(page) = self.pending.take() {
            self.cursor = Some(page.next_cursor);
        }
        self.cursor
    }

    /// Observe for at most 55s, stopping at the first event page or retention gap.
    /// Each transport call remains a valid <=1s zbridge request. Empty windows
    /// retain their confirmed cursor before the next call, including cancellation.
    ///
    /// `request_id` identifies this observation, with `.0`, `.1`, ... appended to
    /// correlate each exchange. Supply a new ID for each invocation (<=124 bytes).
    /// A zero window performs one nonwaiting read with a 2s transport deadline.
    /// The callback runs sequentially and is never invoked for a command.
    pub async fn wait_with<F, Fut>(
        &mut self,
        request_id: Id,
        limit: u16,
        wait_ms: u16,
        mut transport: F,
    ) -> Result<&EventPage, BridgeError>
    where
        F: FnMut(Request) -> Fut,
        Fut: Future<Output = Result<Response, BridgeError>>,
    {
        if wait_ms > MAX_OBSERVATION_MS || request_id.as_str().len() > 124 {
            return Err(BridgeError::invalid(
                "observation requires wait_ms <=55000 and request_id <=124 bytes",
            ));
        }
        if self
            .pending
            .as_ref()
            .is_some_and(|page| page.retention_gap || page.has_more || !page.events.is_empty())
        {
            return Err(BridgeError::new(
                ErrorCode::Conflict,
                "apply or reconcile the pending event page, then advance its cursor",
            ));
        }
        // Validate even a zero-window call before touching the transport.
        Request {
            version: zbridge::VERSION,
            id: request_id.clone(),
            operation: Operation::Events {
                generation: self.generation.clone(),
                cursor: self.cursor,
                limit,
                wait_ms: 0,
            },
        }
        .validate()?;
        self.pending = None;
        let deadline = Instant::now()
            + if wait_ms == 0 {
                SINGLE_READ_TIMEOUT
            } else {
                Duration::from_millis(u64::from(wait_ms))
            };
        let mut last_empty = None;
        let mut round = 0_u16;
        let page = loop {
            let now = Instant::now();
            if wait_ms != 0 && now >= deadline {
                break last_empty.ok_or_else(|| {
                    BridgeError::new(
                        ErrorCode::Unavailable,
                        "observation expired before accepting a response",
                    )
                })?;
            }
            let native_wait = if wait_ms == 0 {
                0
            } else {
                deadline
                    .saturating_duration_since(now)
                    .as_millis()
                    .clamp(1, u128::from(zbridge::MAX_WAIT_MS)) as u16
            };
            let request = Request {
                version: zbridge::VERSION,
                id: Id::new(format!("{request_id}.{round}"))?,
                operation: Operation::Events {
                    generation: self.generation.clone(),
                    cursor: self.cursor,
                    limit,
                    wait_ms: native_wait,
                },
            };
            let response = match timeout_at(deadline, transport(request.clone())).await {
                Ok(response) => response?,
                Err(_) => {
                    break last_empty.ok_or_else(|| {
                        BridgeError::new(
                            ErrorCode::Unavailable,
                            "observation transport deadline elapsed",
                        )
                    })?;
                }
            };
            response.validate_for(&request)?;
            let page = match response.body {
                ResponseBody::Events(page) => page,
                ResponseBody::Error(error) => return Err(error),
                _ => {
                    return Err(BridgeError::invalid(
                        "observation received a non-event response",
                    ));
                }
            };
            // Capture the first live boundary, but keep event delivery separate
            // from frontend consumption. A drop after this point cannot skip it.
            if self.cursor.is_none() {
                // start_cursor is the retained floor, which may precede a live
                // read. Resume from the first delivered event (or empty head).
                self.cursor = Some(
                    page.events
                        .first()
                        .map_or(page.next_cursor, |event| event.sequence),
                );
            }
            if !page.events.is_empty() || page.retention_gap || page.has_more || wait_ms == 0 {
                break page;
            }
            self.cursor = Some(page.next_cursor);
            last_empty = Some(page);
            round += 1;
            // A host may return an empty page early. Do not turn that into a
            // busy polling loop or issue unbounded requests within one window.
            sleep_until((now + Duration::from_millis(u64::from(native_wait))).min(deadline)).await;
        };
        self.pending = Some(page);
        Ok(self
            .pending
            .as_ref()
            .expect("accepted event page is retained"))
    }
}

#[cfg(test)]
#[path = "observation/tests.rs"]
mod tests;
