//! Per-tenant staleness, exported for scraping.
//!
//! The broker is the detector because a dead client cannot report its own death.
//! "The Mac slept", "plant is down", "the job was never scheduled on this host"
//! all produce *no* client ledger line at all, and an absent line is
//! indistinguishable from a healthy quiet period. The broker has the opposite
//! vantage: it knows every tenant and when each last spoke, so going dark is a
//! positive signal here.
//!
//! Two ages, because they answer different questions and only one of them is a
//! liveness check:
//!
//! - `seal_last_contact_age_seconds` is the liveness signal. It advances on any
//!   authenticated request, so a tenant with nothing new to upload still reports
//!   healthy. **This is the series to alert on**, together with `absent()` — a
//!   tenant that has never spoken since the broker started has no series at all,
//!   which is exactly as alarming as a stale one.
//! - `seal_last_upload_age_seconds` is durability progress. It is emitted only
//!   once a tenant has actually stored something, because an age measured from
//!   process start would be a fabricated reading, and seeding it at first
//!   contact would report an upload that never happened.
//!
//! State is in memory and does not survive a restart, which is safe in exactly
//! one direction: a restart erases the series rather than resetting it to zero,
//! so a tenant that was already dark stays caught by `absent()`. Nothing here is
//! allowed to look healthier than the truth.

use crate::tenant::Tenant;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
struct TenantState {
    last_contact: Option<Instant>,
    last_upload: Option<Instant>,
    uploaded: u64,
    unchanged: u64,
    failed: u64,
    bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Uploaded,
    Unchanged,
    Failed,
}

#[derive(Debug, Default)]
struct StoreView {
    objects: u64,
    bytes: u64,
    at: Option<Instant>,
}

#[derive(Default)]
pub struct Metrics {
    tenants: Mutex<BTreeMap<Tenant, TenantState>>,
    store: Mutex<StoreView>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    /// Any authenticated request from a tenant, whatever it asked for.
    pub fn contact(&self, tenant: &Tenant) {
        self.with(tenant, |state| state.last_contact = Some(Instant::now()));
    }

    pub fn record(&self, tenant: &Tenant, outcome: Outcome, bytes: u64) {
        self.with(tenant, |state| match outcome {
            Outcome::Uploaded => {
                state.uploaded += 1;
                state.bytes += bytes;
                state.last_upload = Some(Instant::now());
            }
            // An unchanged seal is proof the tenant is reconciling, but it stored
            // nothing, so it must not advance durability progress.
            Outcome::Unchanged => state.unchanged += 1,
            Outcome::Failed => state.failed += 1,
        });
    }

    pub fn observe_store(&self, objects: u64, bytes: u64) {
        *self.store.lock().unwrap() = StoreView {
            objects,
            bytes,
            at: Some(Instant::now()),
        };
    }

    fn with(&self, tenant: &Tenant, edit: impl FnOnce(&mut TenantState)) {
        let mut tenants = self.tenants.lock().unwrap();
        edit(tenants.entry(tenant.clone()).or_default());
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        let secs = |at: Option<Instant>| at.map(|at| at.elapsed().as_secs());

        out.push_str(
            "# HELP seal_last_contact_age_seconds Seconds since this tenant last reached the broker.\n\
             # TYPE seal_last_contact_age_seconds gauge\n",
        );
        let tenants = self.tenants.lock().unwrap();
        for (tenant, state) in tenants.iter() {
            if let Some(age) = secs(state.last_contact) {
                out.push_str(&format!(
                    "seal_last_contact_age_seconds{{tenant=\"{tenant}\"}} {age}\n"
                ));
            }
        }

        out.push_str(
            "# HELP seal_last_upload_age_seconds Seconds since this tenant last stored a seal.\n\
             # TYPE seal_last_upload_age_seconds gauge\n",
        );
        for (tenant, state) in tenants.iter() {
            if let Some(age) = secs(state.last_upload) {
                out.push_str(&format!(
                    "seal_last_upload_age_seconds{{tenant=\"{tenant}\"}} {age}\n"
                ));
            }
        }

        out.push_str(
            "# HELP seal_uploads_total Seal uploads by outcome.\n\
             # TYPE seal_uploads_total counter\n",
        );
        for (tenant, state) in tenants.iter() {
            for (outcome, count) in [
                ("uploaded", state.uploaded),
                ("unchanged", state.unchanged),
                ("failed", state.failed),
            ] {
                out.push_str(&format!(
                    "seal_uploads_total{{tenant=\"{tenant}\",outcome=\"{outcome}\"}} {count}\n"
                ));
            }
        }

        out.push_str(
            "# HELP seal_upload_bytes_total Bytes stored on behalf of this tenant.\n\
             # TYPE seal_upload_bytes_total counter\n",
        );
        for (tenant, state) in tenants.iter() {
            out.push_str(&format!(
                "seal_upload_bytes_total{{tenant=\"{tenant}\"}} {}\n",
                state.bytes
            ));
        }
        drop(tenants);

        let store = self.store.lock().unwrap();
        if let Some(age) = secs(store.at) {
            out.push_str(&format!(
                "# HELP seal_store_objects Objects in the seal store at the last listing.\n\
                 # TYPE seal_store_objects gauge\n\
                 seal_store_objects {}\n\
                 # HELP seal_store_bytes Bytes in the seal store at the last listing.\n\
                 # TYPE seal_store_bytes gauge\n\
                 seal_store_bytes {}\n\
                 # HELP seal_store_listing_age_seconds Age of the store listing above.\n\
                 # TYPE seal_store_listing_age_seconds gauge\n\
                 seal_store_listing_age_seconds {age}\n",
                store.objects, store.bytes
            ));
        }
        out
    }
}

/// How long a listing is served from cache before the store is asked again.
pub const LISTING_TTL: Duration = Duration::from_secs(300);

#[cfg(test)]
mod tests {
    use super::*;

    fn tenant(name: &str) -> Tenant {
        Tenant::from_node_name(name).unwrap()
    }

    #[test]
    fn a_tenant_that_has_never_spoken_has_no_series() {
        let metrics = Metrics::new();
        let text = metrics.render();
        assert!(!text.contains("seal_last_contact_age_seconds{"), "{text}");
        assert!(!text.contains("seal_last_upload_age_seconds{"), "{text}");
    }

    // The distinction the alert depends on: a tenant reconciling with nothing to
    // send is alive, and must not be reported as having stored something.
    #[test]
    fn contact_without_an_upload_reports_liveness_and_not_durability() {
        let metrics = Metrics::new();
        let mac = tenant("CB14957.hs.cnb.rocks.");
        metrics.contact(&mac);
        metrics.record(&mac, Outcome::Unchanged, 0);
        let text = metrics.render();
        assert!(text.contains("seal_last_contact_age_seconds{tenant=\"cb14957\"} 0"));
        assert!(!text.contains("seal_last_upload_age_seconds{tenant=\"cb14957\""));
        assert!(text.contains("seal_uploads_total{tenant=\"cb14957\",outcome=\"unchanged\"} 1"));
    }

    #[test]
    fn an_upload_reports_both_ages_and_its_bytes() {
        let metrics = Metrics::new();
        let mac = tenant("cb14957");
        metrics.contact(&mac);
        metrics.record(&mac, Outcome::Uploaded, 15_200_000);
        metrics.record(&mac, Outcome::Failed, 0);
        let text = metrics.render();
        assert!(text.contains("seal_last_upload_age_seconds{tenant=\"cb14957\"} 0"));
        assert!(text.contains("seal_upload_bytes_total{tenant=\"cb14957\"} 15200000"));
        assert!(text.contains("seal_uploads_total{tenant=\"cb14957\",outcome=\"failed\"} 1"));
    }

    #[test]
    fn tenants_are_reported_separately() {
        let metrics = Metrics::new();
        metrics.contact(&tenant("cb14957"));
        metrics.contact(&tenant("computer-dev-1"));
        let text = metrics.render();
        assert!(text.contains("seal_last_contact_age_seconds{tenant=\"cb14957\"}"));
        assert!(text.contains("seal_last_contact_age_seconds{tenant=\"computer-dev-1\"}"));
    }

    #[test]
    fn the_store_view_is_reported_only_once_it_has_been_listed() {
        let metrics = Metrics::new();
        assert!(!metrics.render().contains("seal_store_objects"));
        metrics.observe_store(9_421, 7_458_175_928);
        let text = metrics.render();
        assert!(text.contains("seal_store_objects 9421"));
        assert!(text.contains("seal_store_bytes 7458175928"));
    }
}
