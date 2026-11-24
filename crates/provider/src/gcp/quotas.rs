//! Whether the project's quota covers one more machine.
//!
//! The same distinction Azure and AWS draw, and Google states it most
//! plainly of the three: `CPUS` and `PREEMPTIBLE_CPUS` are separate
//! per-region quotas with separate limits, and **spot spends only the
//! second**. Checking a spot request against `CPUS` would refuse machines
//! the project can genuinely run — which, on a new project whose on-demand
//! limit is small, is most of them.
//!
//! Unlike AWS's, a Compute Engine quota publishes **both halves**: `limit`
//! and `usage`, in one read, exactly as an Azure usage list does. So there
//! is nothing to count and nothing to infer.

use crate::{CapacityMode, ProviderError, QuotaUnit};

use super::compute::{Quota, RegionInfo};

/// Quota covering every on-demand vCPU in a region.
pub const CPUS_QUOTA: &str = "CPUS";

/// Quota covering every interruptible vCPU in a region.
///
/// Spot has its own pool and does not touch [`CPUS_QUOTA`], which is why
/// [`Quotas::require_capacity_for`] takes the capacity mode.
pub const PREEMPTIBLE_CPUS_QUOTA: &str = "PREEMPTIBLE_CPUS";

/// Status of a region that can be deployed into.
pub const REGION_UP: &str = "UP";

/// A region's quotas, ready to be asked whether one more machine fits.
#[derive(Debug, Clone, Default)]
pub struct Quotas {
    entries: Vec<(String, f64, f64)>,
}

impl Quotas {
    /// Builds a lookup from one region's own answer.
    #[must_use]
    pub fn from_region(region: &RegionInfo) -> Self {
        Self {
            entries: region
                .quotas
                .iter()
                .map(
                    |Quota {
                         metric,
                         usage,
                         limit,
                     }| (metric.clone(), *usage, *limit),
                )
                .collect(),
        }
    }

    /// The `(used, limit)` pair for one quota.
    #[must_use]
    pub fn get(&self, metric: &str) -> Option<(u32, u32)> {
        self.entries
            .iter()
            .find(|(name, _, _)| name == metric)
            .map(|(_, usage, limit)| (whole(*usage), whole(*limit)))
    }

    /// Refuses unless `vcpus` virtual CPUs fit under `metric` after releasing
    /// `releasing_vcpus` from the same pool (non-zero when resizing a running
    /// instance in place).
    ///
    /// A quota the region does not publish is not treated as unlimited: it
    /// is a metric name this driver got wrong, and provisioning against it
    /// would fail later with an opaque `QUOTA_EXCEEDED` instead of here with
    /// the name.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::QuotaExceeded`] when the headroom is not
    /// there, or [`ProviderError::Unavailable`] when the quota is unknown.
    pub fn require_after_release(
        &self,
        metric: &str,
        region: &str,
        vcpus: u32,
        releasing_vcpus: u32,
    ) -> Result<(), ProviderError> {
        let Some((used, limit)) = self.get(metric) else {
            return Err(ProviderError::Unavailable {
                machine_type: metric.to_owned(),
                region: region.to_owned(),
                reason: "the project publishes no quota under this name".to_owned(),
            });
        };

        let effective_used = used.saturating_sub(releasing_vcpus);
        if effective_used.saturating_add(vcpus) <= limit {
            return Ok(());
        }

        Err(ProviderError::QuotaExceeded {
            unit: QuotaUnit::Vcpus,
            quota: metric.to_owned(),
            region: region.to_owned(),
            limit,
            used: effective_used,
            requested: vcpus,
        })
    }

    /// Refuses unless `vcpus` more virtual CPUs fit under `metric`.
    ///
    /// # Errors
    ///
    /// Returns [`ProviderError::QuotaExceeded`] when the headroom is not
    /// there, or [`ProviderError::Unavailable`] when the quota is unknown.
    pub fn require(&self, metric: &str, region: &str, vcpus: u32) -> Result<(), ProviderError> {
        self.require_after_release(metric, region, vcpus, 0)
    }

    /// Refuses unless the pool that funds this capacity mode covers one more
    /// machine of this size.
    ///
    /// # Errors
    ///
    /// Returns whichever check fails.
    pub fn require_capacity_for(
        &self,
        vcpus: u32,
        region: &str,
        mode: CapacityMode,
    ) -> Result<(), ProviderError> {
        self.require_capacity_for_resize(vcpus, 0, region, mode)
    }

    /// Refuses unless the pool that funds `mode` covers `target_vcpus` once
    /// `releasing_vcpus` from the currently running VM are freed.
    ///
    /// # Errors
    ///
    /// Returns whichever check fails.
    pub fn require_capacity_for_resize(
        &self,
        target_vcpus: u32,
        releasing_vcpus: u32,
        region: &str,
        mode: CapacityMode,
    ) -> Result<(), ProviderError> {
        let metric = if mode.is_spot() {
            PREEMPTIBLE_CPUS_QUOTA
        } else {
            CPUS_QUOTA
        };
        self.require_after_release(metric, region, target_vcpus, releasing_vcpus)
    }
}

/// A quota amount as a whole count.
///
/// The API states them as floating point because some quotas are fractional;
/// a vCPU quota never is, and truncating toward zero is the conservative
/// reading of a limit.
const fn whole(amount: f64) -> u32 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a vCPU quota is a small non-negative count"
    )]
    let whole = amount.max(0.0) as u32;
    whole
}

/// Whether a region can be deployed into at all.
#[must_use]
pub fn is_up(region: &RegionInfo) -> bool {
    region.status == REGION_UP
}

#[cfg(test)]
mod tests {
    use super::{Quotas, is_up};
    use crate::gcp::compute::RegionInfo;
    use crate::{CapacityMode, ProviderError};

    fn region(fixture: &str) -> RegionInfo {
        serde_json::from_str(fixture).expect("the region fixture parses")
    }

    fn quotas() -> Quotas {
        Quotas::from_region(&region(include_str!("../../fixtures/gcp/region.json")))
    }

    #[test]
    fn a_regions_own_answer_states_both_halves() {
        // Unlike AWS, where the limit and the usage come from two services.
        assert_eq!(quotas().get("CPUS"), Some((2, 8)));
        assert_eq!(quotas().get("PREEMPTIBLE_CPUS"), Some((0, 4)));
    }

    #[test]
    fn spot_spends_the_preemptible_pool_and_nothing_else() {
        let quotas = quotas();
        // Six more on-demand vCPUs fit under a limit of eight with two used.
        quotas
            .require_capacity_for(6, "us-central1", CapacityMode::OnDemand)
            .expect("six fit");
        let error = quotas
            .require_capacity_for(7, "us-central1", CapacityMode::OnDemand)
            .expect_err("seven do not");
        assert!(matches!(
            error,
            ProviderError::QuotaExceeded { ref quota, used: 2, limit: 8, .. } if quota == "CPUS"
        ));

        // The spot pool is untouched by on-demand usage.
        quotas
            .require_capacity_for(4, "us-central1", CapacityMode::Spot)
            .expect("the whole preemptible pool is free");
    }

    #[test]
    fn a_spent_preemptible_pool_refuses_spot_while_on_demand_still_works() {
        let spent = Quotas::from_region(&region(include_str!(
            "../../fixtures/gcp/region_preemptible_spent.json"
        )));

        let error = spent
            .require_capacity_for(2, "us-central1", CapacityMode::Spot)
            .expect_err("the preemptible pool is spent");
        assert!(matches!(
            error,
            ProviderError::QuotaExceeded { ref quota, .. } if quota == "PREEMPTIBLE_CPUS"
        ));
        spent
            .require_capacity_for(2, "us-central1", CapacityMode::OnDemand)
            .expect("the on-demand pool is funded separately");
    }

    #[test]
    fn a_quota_the_region_does_not_publish_is_an_error_rather_than_unlimited() {
        let error = quotas()
            .require("NOT_A_METRIC", "us-central1", 2)
            .expect_err("an unknown metric must not read as unlimited");
        assert!(matches!(error, ProviderError::Unavailable { .. }));
    }

    #[test]
    fn a_region_that_is_not_up_cannot_be_deployed_into() {
        assert!(is_up(&region(include_str!(
            "../../fixtures/gcp/region.json"
        ))));
        assert!(!is_up(&region(include_str!(
            "../../fixtures/gcp/region_down.json"
        ))));
    }

    #[test]
    fn resize_subtracts_the_running_vms_own_vcpus_from_the_pool() {
        let quotas = quotas(); // CPUS: 2 used / 8 limit
        // Requesting 8 new vCPUs fails (2 + 8 > 8), but resizing a running
        // 2-vCPU VM to 8 vCPUs succeeds ((2 - 2) + 8 <= 8).
        assert!(
            quotas
                .require_capacity_for(8, "us-central1", CapacityMode::OnDemand)
                .is_err()
        );
        quotas
            .require_capacity_for_resize(8, 2, "us-central1", CapacityMode::OnDemand)
            .expect("releasing the running VM's 2 vCPUs makes room for 8");
    }
}
