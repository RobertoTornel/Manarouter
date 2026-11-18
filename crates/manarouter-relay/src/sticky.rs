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

/// How long a failover may keep winning before the configured order is tried again.
///
/// The sliding window alone is not enough: a combo that failed over once would keep serving the
/// fallback for as long as traffic kept the window alive, so a provider that recovered a minute
/// later would never be used again. A pin is therefore bounded by *total* age, independent of the
/// window, so it cannot become permanent.
///
/// The bound is deliberately generous relative to a cooldown cycle: sticky is what stops the
/// ping-pong of two flaky providers trading the pin back and forth, so it must outlast a few
/// cooldown windows. Thirty minutes is long enough that a recovered primary is retried promptly
/// (a cooling step is demoted on its own anyway) while a burst stays on the step that answered.
const MAX_PIN_AGE: Duration = Duration::from_secs(1800);

#[derive(Clone, Debug)]
struct Pin {
    provider_id: String,
    model: String,
    level: u8,
    /// Identifies the combo's step list at pin time, so an edited combo cannot inherit a stale
    /// preference. Derived from `steps` by [`StickySuccess::steps_key`].
    steps_key: u64,
    expires_at: Instant,
    touched_at: Instant,
    pinned_at: Instant,
}

/// A cheap, stable identity for a combo's current step list.
///
/// Collisions are acceptable: a collision means a pin keeps being consulted for a combo saved
/// differently, and the configured order is simply what decides — it cannot misroute.
fn steps_key(steps: &[StepPlan]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for step in steps {
        step.provider_id.as_str().hash(&mut hasher);
        step.model.hash(&mut hasher);
        step.level.hash(&mut hasher);
    }
    hasher.finish()
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
    ///
    /// A pin is a *failover* memory, not a promotion. It is honoured for at most MAX_PIN_AGE from
    /// the moment the failover happened, so the configured order is retried regularly and a
    /// provider that recovers is used again. Without that cap a single bad minute would pin a
    /// combo to its last step for as long as traffic kept the sliding window alive, which is the
    /// opposite of what the operator asked for when they put a provider first.
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
            if pin.expires_at <= now || now.duration_since(pin.pinned_at) >= MAX_PIN_AGE {
                pins.remove(combo_id.as_str());
                return None;
            }
            if pin.steps_key != steps_key(steps) {
                // The combo was edited since the pin was made: the preference describes a step
                // list that no longer exists, so it is dropped for one request and rebuilt from
                // the new order.
                pins.remove(combo_id.as_str());
                return None;
            }
            (
                pin.provider_id.clone(),
                pin.model.clone(),
                pin.level,
                pin.expires_at,
            )
        };
        // Provider/account eligibility work is intentionally outside the cache lock.
        let index = steps.iter().position(|step| {
            step.provider_id.as_str() == identity.0
                && step.model == identity.1
                && step.level == identity.2
                && eligible(step)
        });
        let mut pins = self
            .pins
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if let Some(pin) = pins.get_mut(combo_id.as_str()) {
            if pin.provider_id != identity.0
                || pin.model != identity.1
                || pin.level != identity.2
                || pin.expires_at != identity.3
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
    ///
    /// `steps` is the combo's current step list; it is captured in the pin so an edited combo
    /// (any reorder, model swap or strategy change) invalidates the preference.
    pub fn record_success(
        &self,
        combo_id: &ComboId,
        steps: &[StepPlan],
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
        // A repeat success on the step already pinned extends the sliding window but must not
        // restart the age cap, or the cap would never be reached under steady traffic.
        let pinned_at = pins
            .get(combo_id.as_str())
            .filter(|pin| {
                pin.provider_id == step.provider_id.as_str()
                    && pin.model == step.model
                    && pin.level == step.level
            })
            .map(|pin| pin.pinned_at)
            .unwrap_or(now);
        pins.insert(
            combo_id.as_str().to_owned(),
            Pin {
                provider_id: step.provider_id.as_str().to_owned(),
                model: step.model.clone(),
                level: step.level,
                steps_key: steps_key(steps),
                expires_at: now + window,
                touched_at: now,
                pinned_at,
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
            level: mr_core::dialect::ReasoningLevel::High.rank(),
            levels: 0,
            default_level: mr_core::dialect::ReasoningLevel::High.rank(),
            reasoning_json: Default::default(),
            input_modalities: 1,
            pricing: None,
            context_length: None,
            accounts: None,
        }
    }

    #[test]
    fn preference_expires_and_success_refreshes_sliding_window() {
        let cache = StickySuccess::default();
        let combo = ComboId::from_raw("combo");
        let steps = vec![step("a", "m"), step("b", "m")];
        let start = Instant::now();
        cache.record_success(&combo, &steps, &steps[1], Duration::from_secs(30), start);
        assert_eq!(
            cache.preferred_index(&combo, &steps, |_| true, start),
            Some(1)
        );
        let second_success = start + Duration::from_secs(20);
        cache.record_success(
            &combo,
            &steps,
            &steps[1],
            Duration::from_secs(30),
            second_success,
        );
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
        cache.record_success(&combo, &steps, &steps[0], Duration::from_secs(30), now);
        assert_eq!(
            cache.preferred_index(&combo, &steps, |s| s.provider_id.as_str() == "b", now),
            None
        );
        // The fallback becomes the pin, and it is honoured while the step ahead of it is down.
        cache.record_success(&combo, &steps, &steps[1], Duration::from_secs(30), now);
        assert_eq!(
            cache.preferred_index(&combo, &steps, |s| s.provider_id.as_str() == "b", now),
            Some(1)
        );
        cache.clear(&combo);
        assert_eq!(cache.preferred_index(&combo, &steps, |_| true, now), None);
    }

    #[test]
    fn a_failover_pin_expires_even_while_traffic_keeps_refreshing_it() {
        let cache = StickySuccess::default();
        let combo = ComboId::from_raw("combo");
        let steps = vec![step("a", "m"), step("b", "m"), step("c", "m")];
        let start = Instant::now();
        // The combo failed over to its last step while the first two were unusable.
        cache.record_success(&combo, &steps, &steps[2], Duration::from_secs(1800), start);
        // Steady traffic keeps the sliding window alive, but not the age cap.
        let mut now = start;
        while now.duration_since(start) < MAX_PIN_AGE - Duration::from_secs(10) {
            now += Duration::from_secs(10);
            cache.record_success(&combo, &steps, &steps[2], Duration::from_secs(1800), now);
            assert_eq!(
                cache.preferred_index(&combo, &steps, |_| true, now),
                Some(2)
            );
        }
        // Once the cap is reached the configured order is tried again.
        let after = start + MAX_PIN_AGE;
        assert_eq!(cache.preferred_index(&combo, &steps, |_| true, after), None);
        // A fresh failover starts a fresh cap.
        cache.record_success(&combo, &steps, &steps[2], Duration::from_secs(1800), after);
        assert_eq!(
            cache.preferred_index(&combo, &steps, |_| true, after + Duration::from_secs(1)),
            Some(2)
        );
    }

    #[test]
    fn pin_uses_provider_model_not_step_index_and_cache_is_bounded() {
        let cache = StickySuccess::default();
        let steps = [step("a", "m"), step("b", "m")];
        let now = Instant::now();
        let combo = ComboId::from_raw("combo");
        cache.record_success(&combo, &steps, &steps[1], Duration::from_secs(30), now);
        assert_eq!(
            cache.preferred_index(&combo, &steps, |_| true, now),
            Some(1),
            "the pin prefers the provider/model that answered"
        );
        // An edited combo (here: a reorder) invalidates the pin: the preference described a step
        // list that no longer exists, so it is dropped and the configured order decides.
        let reordered = [steps[1].clone(), steps[0].clone()];
        assert_eq!(
            cache.preferred_index(&combo, &reordered, |_| true, now),
            None,
            "a changed combo must not inherit a stale pin"
        );
        for i in 0..(MAX_ENTRIES + 5) {
            cache.record_success(
                &ComboId::from_raw(format!("c{i}")),
                &steps,
                &steps[0],
                Duration::from_secs(30),
                now,
            );
        }
        assert_eq!(cache.len(), MAX_ENTRIES);
    }
}
