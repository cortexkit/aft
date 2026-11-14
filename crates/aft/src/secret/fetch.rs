//! Addressed fetch secret use, independent of the route that stamped its scopes.

use super::call_context::{current_call_scopes, current_secret_consent_context};
use super::LabelOutcome;
use std::sync::Arc;

#[derive(Debug)]
pub(crate) enum FetchDecision {
    Proceed(super::UseAnswer),
    Refuse(LabelOutcome),
    InvalidPlan(&'static str),
}

pub(crate) fn fetch_consent_refusal(
    label: &str,
    host: &str,
    reason: crate::secret::requests::UseError,
    held: &crate::url_fetch::HeldKeys,
) -> LabelOutcome {
    use crate::secret::requests::UseError;
    if matches!(reason, UseError::Indeterminate) {
        let outcome = crate::secret::outcome_for(label, &crate::secret::UseAnswer::Unavailable);
        return match outcome {
            LabelOutcome::Refuse { code, message } => LabelOutcome::Refuse {
                code,
                message: held.sanitize_detail(&message),
            },
            other => other,
        };
    }
    let message = crate::secret::requests::consent_failure_message(label, host, reason);
    LabelOutcome::Refuse {
        code: crate::secret::SECRET_ACCESS_UNAVAILABLE,
        message: held.sanitize_detail(&message),
    }
}

/// Fetch and proxy requests join the same scope/request-id cards, each with
/// one person-wait deadline shared by every concurrently requested label.
pub(crate) fn use_current_secret_labels(
    vault: Arc<dyn crate::secret::SecretVault>,
    labels: &[String],
    target: &crate::secret::ApiKeyTarget,
    request_target: &str,
    cancellation: &crate::url_fetch::SecretFetchCancellation,
) -> Vec<FetchDecision> {
    if labels.is_empty() {
        return Vec::new();
    }
    let redactions = Arc::new(std::sync::Mutex::new(crate::url_fetch::HeldKeys::default()));
    let refuse = |index: usize, reason| {
        labels
            .iter()
            .map(|_| {
                FetchDecision::Refuse(fetch_consent_refusal(
                    &labels[index],
                    &target.host,
                    reason,
                    &redactions
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                ))
            })
            .collect()
    };
    if let Some(reason) = cancellation.reason() {
        return refuse(0, reason);
    }
    let scopes = match current_call_scopes() {
        Ok((scopes, _)) => scopes,
        Err(reason) => {
            return labels
                .iter()
                .map(|label| FetchDecision::Refuse(crate::secret::refuse_ineligible(label, reason)))
                .collect()
        }
    };
    #[cfg(unix)]
    if vault.can_release_material() && crate::secret_proxy::protect_secret_process().is_err() {
        return refuse(0, crate::secret::requests::UseError::Unavailable);
    }
    let checkpoint = cancellation.clone();
    let coordinator = crate::secret::requests::RequestSecrets::new(
        vault,
        scopes.authorization,
        current_secret_consent_context(),
    )
    .with_request_cancellation(move || checkpoint.is_cancelled());
    let labels = labels.to_vec();
    let target = target.clone();
    let request_target = request_target.to_owned();
    let cancellation = cancellation.clone();
    let observed = redactions.clone();
    let future = async move {
        use futures_util::StreamExt;
        let mut budget = coordinator.budget();
        if let Some(expires) = cancellation.expires_at_ms() {
            budget.expires_at_ms = budget.expires_at_ms.min(expires);
        }
        let mut pending = futures_util::stream::FuturesUnordered::new();
        for (index, label) in labels.iter().enumerate() {
            let target = &target;
            let request_target = &request_target;
            let coordinator = &coordinator;
            let expires = budget.expires_at_ms;
            let observed = &observed;
            pending.push(async move {
                (
                    index,
                    coordinator
                        .use_planned_in_request(
                            label,
                            target,
                            expires,
                            |location| {
                                let location =
                                    secret_rules::location::parse_vault_location(location).ok_or(
                                        crate::secret::requests::UseError::InvalidPlan(
                                            "missing_material",
                                        ),
                                    )?;
                                if !matches!(location, secret_rules::location::Location::Query(_)) {
                                    // A covered deposit reveals its location only with the key, so the vault already recorded one release for that first ask.
                                    return Err(crate::secret::requests::UseError::InvalidPlan(
                                        "missing_material",
                                    ));
                                }
                                let authority = secret_rules::target::ConnectAuthority {
                                    host: target.host.clone(),
                                    port: target.port.unwrap_or(443),
                                };
                                let rule = secret_rules::location::LabelRule {
                                    label: label.clone(),
                                    host: target.host.clone(),
                                    port: 443,
                                    path_prefix: target.path.clone(),
                                    location,
                                };
                                let view = secret_rules::location::RequestView::new(
                                    request_target,
                                    Vec::new(),
                                );
                                let dummy =
                                    [(label.clone(), b"proxy-planning-value-0000".to_vec())];
                                // Without a pending card, the vault reveals a deposit location only when releasing its key, so location-dependent refusals can follow that release.
                                if let Some(refusal) =
                                    secret_rules::location::evaluate_request_with_connect(
                                        &view,
                                        &authority,
                                        &[rule],
                                        &dummy,
                                    )
                                    .refusal
                                {
                                    Err(crate::secret::requests::UseError::InvalidPlan(
                                        refusal.reason(),
                                    ))
                                } else {
                                    Ok(())
                                }
                            },
                            |key| {
                                secret_rules::material::check_admissible(
                                    key,
                                    label,
                                    &secret_rules::location::Location::Query("_".into()),
                                )
                                .map_err(|_| {
                                    crate::secret::requests::UseError::InvalidPlan(
                                        "material_not_admissible",
                                    )
                                })?;
                                observed
                                    .lock()
                                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                                    .hold(label, key)
                                    .map_err(|_| {
                                        crate::secret::requests::UseError::InvalidPlan(
                                            "replacement_discloses_key",
                                        )
                                    })
                            },
                            || async { Ok(()) },
                        )
                        .await,
                )
            });
        }
        let mut answers: Vec<Option<crate::secret::UseAnswer>> =
            (0..labels.len()).map(|_| None).collect();
        while !pending.is_empty() {
            let index = answers.iter().position(Option::is_none).unwrap_or(0);
            let (index, answer) = tokio::select! {
                biased;
                () = cancellation.cancelled() => return Err((index, cancellation.reason().unwrap_or(crate::secret::requests::UseError::Cancelled))),
                () = budget.expired() => return Err((index, crate::secret::requests::UseError::Expired)),
                result = pending.next() => result.ok_or((index, crate::secret::requests::UseError::Unavailable))?,
            };
            if let Some(reason) = cancellation.reason() {
                return Err((index, reason));
            }
            match answer {
                Ok(answer) => answers[index] = Some(answer),
                Err(crate::secret::requests::UseError::InvalidPlan(reason)) => {
                    return Ok(labels
                        .iter()
                        .map(|_| FetchDecision::InvalidPlan(reason))
                        .collect())
                }
                Err(reason) => return Err((index, reason)),
            }
        }
        Ok(labels
            .iter()
            .zip(answers)
            .map(|(label, answer)| {
                let answer = answer.unwrap_or(crate::secret::UseAnswer::Unavailable);
                let outcome = crate::secret::outcome_for(label, &answer);
                if outcome.proceeds() {
                    FetchDecision::Proceed(answer)
                } else {
                    FetchDecision::Refuse(outcome)
                }
            })
            .collect())
    };
    // Blocking fetches run on tool worker threads. A local runtime drives only
    // their coordinator; production card workers stay on the daemon runtime.
    let result = match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(future))
        }
        Ok(_) => Err((0, crate::secret::requests::UseError::Unavailable)),
        Err(_) => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| (0, crate::secret::requests::UseError::Unavailable))
            .and_then(|runtime| {
                let result = runtime.block_on(future);
                // An already-started blocking vault call cannot be aborted; do
                // not make cancellation wait for its transport timeout.
                runtime.shutdown_background();
                result
            }),
    };
    result.unwrap_or_else(|(index, reason)| refuse(index, reason))
}
