//! Bounded, process-local successful-step preference for opted-in priority combos.
//!
//! The cache stores a stable provider/model identity rather than a positional step index so a
//! snapshot reload or step reorder cannot silently redirect a preference.
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use mr_core::ids::ComboId;
use mr_core::snapshot::StepPlan;

const MAX_ENTRIES: usize = 256;

#[derive(Clone, Debug)]
struct Pin {
    provider_id: String,
    model: String,
    expires_at: Instant,
    touched_at: Instant,
}

/// Process-local sticky-success memory. It never performs provider requests or keepalives.
#[derive(Default)]
pub struct StickySuccess {
    pins: Mutex<HashMap<String, Pin>>,
}

impl StickySuccess {
    /// Find a still-valid pin in the current step list.
    ///
    /// The sliding inactivity window is extended on each successful request via `record_success`.
    /// `eligible` must reflect current account, cooldown, model, provider, and auth availability.
    pub fn preferred_index(
        &self,
        combo_id: &ComboId,
        steps: &[StepPlan],
        eligible: impl Fn(&StepPlan) -> bool,
        now: Instant,
    ) -> Option<usize> {
        let identity = {
            let mut pins = self
                .pins
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let pin = pins.get(combo_id.as_str())?;
            if pin.expires_at <= now {
                pins.remove(combo_id.as_str());
                return None;
            }
            (pin.provider_id.clone(), pin.model.clone(), pin.expires_at)
        };
        // Provider/account eligibility work is intentionally outside the cache lock.
        let index = steps.iter().position(|step| {
            step.provider_id.as_str() == identity.0 && step.model == identity.1 && eligible(step)
        });
        let mut pins = self
            .pins
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(pin) = pins.get_mut(combo_id.as_str()) {
            if pin.provider_id != identity.0
                || pin.model != identity.1
                || pin.expires_at != identity.2
            {
                return None;
            }
            if let Some(index) = index {
                return Some(index);
            }
            pins.remove(combo_id.as_str());
        }
        None
    }

    /// Record a successful response and set the expiry to `now + window`.
    pub fn record_success(
        &self,
        combo_id: &ComboId,
        step: &StepPlan,
        window: Duration,
        now: Instant,
    ) {
        if window.is_zero() {
            self.clear(combo_id);
            return;
        }
        let mut pins = self
            .pins
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !pins.contains_key(combo_id.as_str()) && pins.len() >= MAX_ENTRIES {
            // Full-map cleanup is paid only on successful insert at capacity, never on request hits.
            pins.retain(|_, pin| pin.expires_at > now);
            if pins.len() >= MAX_ENTRIES {
                if let Some(oldest) = pins
                    .iter()
                    .min_by_key(|(_, pin)| pin.touched_at)
                    .map(|(key, _)| key.clone())
                {
                    pins.remove(&oldest);
                }
            }
        }
        pins.insert(
            combo_id.as_str().to_owned(),
            Pin {
                provider_id: step.provider_id.as_str().to_owned(),
                model: step.model.clone(),
                expires_at: now + window,
                touched_at: now,
            },
        );
    }

    /// Forget a pin after its step fails.
    pub fn clear(&self, combo_id: &ComboId) {
        self.pins
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .remove(combo_id.as_str());
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.pins.lock().unwrap().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mr_core::ids::{ComboId, ProviderId};
    use mr_core::snapshot::StepPlan;

    fn step(provider: &str, model: &str) -> StepPlan {
        StepPlan {
            provider_index: 0,
            provider_id: ProviderId::from_raw(provider),
            model: model.into(),
            dialect: mr_core::Dialect::OpenAi,
            auth: mr_core::dialect::AuthKind::Bearer,
            base_url: format!("https://{provider}.example/v1"),
            connect_timeout_ms: 100,
            ttfb_timeout_ms: 100,
            idle_timeout_ms: 100,
            total_timeout_ms: 0,
        }
    }

    #[test]
    fn preference_expires_and_success_refreshes_sliding_window() {
        let cache = StickySuccess::default();
        let combo = ComboId::from_raw("combo");
        let steps = vec![step("a", "m"), step("b", "m")];
        let start = Instant::now();
        cache.record_success(&combo, &steps[1], Duration::from_secs(30), start);
        assert_eq!(
            cache.preferred_index(&combo, &steps, |_| true, start),
            Some(1)
        );
        let second_success = start + Duration::from_secs(20);
        cache.record_success(&combo, &steps[1], Duration::from_secs(30), second_success);
        assert_eq!(
            cache.preferred_index(&combo, &steps, |_| true, second_success),
            Some(1)
        );
        assert_eq!(
            cache.preferred_index(&combo, &steps, |_| true, start + Duration::from_secs(49)),
            Some(1)
        );
        assert_eq!(
            cache.preferred_index(&combo, &steps, |_| true, start + Duration::from_secs(51)),
            None
        );
    }

    #[test]
    fn unavailable_pin_is_invalidated_and_fallback_can_become_new_pin() {
        let cache = StickySuccess::default();
        let combo = ComboId::from_raw("combo");
        let steps = vec![step("a", "m"), step("b", "m")];
        let now = Instant::now();
        cache.record_success(&combo, &steps[0], Duration::from_secs(30), now);
        assert_eq!(
            cache.preferred_index(&combo, &steps, |s| s.provider_id.as_str() == "b", now),
            None
        );
        cache.record_success(&combo, &steps[1], Duration::from_secs(30), now);
        assert_eq!(
            cache.preferred_index(&combo, &steps, |_| true, now),
            Some(1)
        );
        cache.clear(&combo);
        assert_eq!(cache.preferred_index(&combo, &steps, |_| true, now), None);
    }

    #[test]
    fn pin_uses_provider_model_not_step_index_and_cache_is_bounded() {
        let cache = StickySuccess::default();
        let steps = [step("a", "m"), step("b", "m")];
        let now = Instant::now();
        let combo = ComboId::from_raw("combo");
        cache.record_success(&combo, &steps[1], Duration::from_secs(30), now);
        let reordered = [steps[1].clone(), steps[0].clone()];
        assert_eq!(
            cache.preferred_index(&combo, &reordered, |_| true, now),
            Some(0)
        );
        for i in 0..(MAX_ENTRIES + 5) {
            cache.record_success(
                &ComboId::from_raw(format!("c{i}")),
                &steps[0],
                Duration::from_secs(30),
                now,
            );
        }
        assert_eq!(cache.len(), MAX_ENTRIES);
    }
}
