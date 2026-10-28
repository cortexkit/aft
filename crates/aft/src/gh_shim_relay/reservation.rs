use super::*;
use crate::gh_shim_speech::partner::{QueuedBotResponse, QueuedResponseError, ReservedBotRequest};
use crate::gh_shim_speech::{Code, Refusal};
use subc_client_rs::consumer::RequestBody;

struct LeasePayload {
    consumer: Arc<subc_client_rs::SubcConsumer>,
    leases: Arc<RouteLeases>,
    key: String,
    _operation: tokio::sync::OwnedSemaphorePermit,
}

struct ReservedLease {
    payload: Option<LeasePayload>,
    runtime: tokio::runtime::Handle,
}

impl Drop for ReservedLease {
    fn drop(&mut self) {
        if let Some(payload) = self.payload.take() {
            // Cleanup owns the acquired lease independently of the caller. A
            // canceled reply cannot interrupt registration or release bookkeeping.
            let _ = self.runtime.spawn(async move {
                let _operation = payload._operation;
                payload
                    .leases
                    .release(&payload.key, &payload.consumer)
                    .await;
            });
        }
    }
}

async fn clear_failed_slot(
    slot: &Arc<tokio::sync::Mutex<Option<Arc<subc_client_rs::SubcConsumer>>>>,
    failed: &Arc<subc_client_rs::SubcConsumer>,
) {
    let mut current = slot.lock().await;
    if current
        .as_ref()
        .is_some_and(|consumer| Arc::ptr_eq(consumer, failed))
    {
        *current = None;
    }
}

fn unavailable(reason: &str) -> Refusal {
    Refusal::new(Code::AuthorityUnavailable, reason)
}

fn opening_refusal(error: TransportError, expected: &ScopeSelector) -> Refusal {
    match error {
        TransportError::ScopeEnded { selector, .. } if &selector == expected => {
            Refusal::new(Code::ScopeEnded, "ended")
        }
        TransportError::ScopeEnded { .. } => Refusal::new(Code::ScopeUnknown, "unverifiable"),
        TransportError::Refused { code, .. } | TransportError::RefusedRetryable { code, .. } => {
            if code.starts_with("scope_") {
                Refusal::new(Code::ScopeUnknown, code)
            } else {
                unavailable(&code)
            }
        }
        TransportError::Unavailable(_) | TransportError::OutcomeUnknown(_) => {
            unavailable("relay_unavailable")
        }
    }
}

impl SubcRelayTransport {
    pub(super) async fn reserve_scoped(
        &self,
        project: &str,
        session: &str,
        scope: &ScopeStamp,
        body: Value,
        deadline: tokio::time::Instant,
    ) -> Result<ReservedBotRequest, Refusal> {
        let bytes = serde_json::to_vec(&body)
            .map_err(|_| Refusal::new(Code::PolicyDenied, "payload_denied"))?;
        let body = RequestBody::new(bytes)
            .map_err(|_| Refusal::new(Code::PolicyDenied, "payload_denied"))?;
        let consumer = tokio::time::timeout_at(deadline, self.consumer())
            .await
            .map_err(|_| unavailable("reservation_timeout"))?
            .map_err(|_| unavailable("relay_unavailable"))?;
        let target = subc_protocol::RouteTarget::ToolProvider {
            module_id: PLEXUS_MODULE_ID.into(),
        };
        let identity = subc_protocol::BindIdentity::new(
            if project.is_empty() { "/" } else { project }.to_string(),
            "aft-gh-relay",
            session.to_string(),
        );
        let selector = scope_selector(scope);
        let key = route_lease_key(&consumer, &target, &identity, Some(&selector))
            .map_err(|_| unavailable("snapshot_invalid"))?;
        let operation = RELAY_OPERATIONS
            .clone()
            .try_acquire_owned()
            .map_err(|_| unavailable("dispatch_queue_full"))?;
        if tokio::time::Instant::now() >= deadline {
            return Err(unavailable("reservation_timeout"));
        }
        let route_open_deadline = self.route_open_deadline;
        let leases = self.leases.clone();
        let slot = self.consumer.clone();
        let (result, receiver) = tokio::sync::oneshot::channel();
        // This owner may open/register/reserve after its requester is dropped,
        // but never sends a mutation. Only the returned slot can do that in SQL.
        let _ = tokio::spawn(async move {
            leases.acquire(&key).await;
            let lease = ReservedLease {
                runtime: tokio::runtime::Handle::current(),
                payload: Some(LeasePayload {
                    consumer: consumer.clone(),
                    leases: leases.clone(),
                    key: key.clone(),
                    _operation: operation,
                }),
            };
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if result.is_closed() || remaining.is_zero() {
                let _ = result.send(Err(unavailable("reservation_timeout")));
                return;
            }
            let open_limit = route_open_deadline.unwrap_or(remaining).min(remaining);
            let options = subc_client_rs::CallOptions {
                consumer_identity: crate::launch_nonce::consumer_identity(),
                timeout: open_limit,
                route_retry_deadline: open_limit,
                ..Default::default()
            };
            let route = match consumer
                .open_route_scoped(target, identity, selector.clone(), options.clone())
                .await
            {
                Ok(route) => route,
                Err(error) => {
                    let refusal = if let Some(error) = scope_open_refusal(&error, &selector) {
                        opening_refusal(error, &selector)
                    } else if let subc_client_rs::CallError::Module(body) = error {
                        unavailable(&body.code)
                    } else {
                        clear_failed_slot(&slot, &consumer).await;
                        unavailable(if tokio::time::Instant::now() >= deadline {
                            "reservation_timeout"
                        } else {
                            "relay_unavailable"
                        })
                    };
                    let _ = result.send(Err(refusal));
                    drop(lease);
                    return;
                }
            };
            leases.opened(&key, route.clone()).await;
            if result.is_closed() {
                drop(lease);
                return;
            }
            let reservation = match consumer.reserve(&route, deadline).await {
                Ok(reservation) => reservation,
                Err(_) => {
                    clear_failed_slot(&slot, &consumer).await;
                    let _ = result.send(Err(unavailable(
                        if tokio::time::Instant::now() >= deadline {
                            "reservation_timeout"
                        } else {
                            "route_closed"
                        },
                    )));
                    drop(lease);
                    return;
                }
            };
            let reserved = ReservedBotRequest::new(move || {
                // SDK 0.32.7 synchronously consumes a pre-reserved writer slot.
                // No socket await or spawned send can escape the final DB gate.
                let (_correlation, pending) = reservation.send(body, options);
                Ok(QueuedBotResponse::new(async move {
                    let _lease = lease;
                    match pending.await {
                        Ok(bytes) => serde_json::from_slice(&bytes)
                            .map_err(|_| QueuedResponseError::Unknown("malformed_reply".into())),
                        Err(subc_client_rs::CallError::NotSent(_)) => {
                            clear_failed_slot(&slot, &consumer).await;
                            Err(QueuedResponseError::NotSent(unavailable(
                                if tokio::time::Instant::now() >= deadline {
                                    "reservation_timeout"
                                } else {
                                    "route_closed"
                                },
                            )))
                        }
                        Err(subc_client_rs::CallError::Module(error)) => Err(
                            QueuedResponseError::Unknown(format!("provider_error_{}", error.code)),
                        ),
                        Err(_) => {
                            clear_failed_slot(&slot, &consumer).await;
                            Err(QueuedResponseError::Unknown(
                                if tokio::time::Instant::now() >= deadline {
                                    "reply_timeout"
                                } else {
                                    "connection_lost"
                                }
                                .into(),
                            ))
                        }
                    }
                }))
            });
            // A dropped receiver drops an unsent SDK reservation and its lease.
            let _ = result.send(Ok(reserved));
        });
        // Bound the caller, not the owner of a possibly successful route open.
        // That owner must finish registration and cleanup even after timeout.
        tokio::time::timeout_at(deadline, receiver)
            .await
            .map_err(|_| unavailable("reservation_timeout"))?
            .map_err(|_| unavailable("relay_unavailable"))?
    }
}
