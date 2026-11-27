//! Private relay ledger, separate from the GitHub account namespace.
use super::{Database, MAX_ACCOUNTS, Store};
use crate::{
    portal::{Problem, relay_accounting::Config},
    user_policy::{MonthlyTrafficAllowances, TrafficFrame, TrafficQuota, traffic_period_at},
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Bindings {
    issuer: String,
    utc_offset_minutes: i32,
    users: BTreeMap<String, String>,
}

impl Store {
    pub(in crate::portal) fn is_external_accounting(&self) -> bool {
        self.database.external.is_some()
    }
}

pub(in crate::portal) struct MonthlyLedger {
    store: Store,
    paid: HashSet<String>,
    offset: i32,
    allowances: MonthlyTrafficAllowances,
}

impl MonthlyLedger {
    pub(in crate::portal) fn open(config: &Config) -> crate::Result<Self> {
        let mut store = Store::open(&config.state_dir)?;
        if !store.database.accounts.is_empty() {
            return Err("external accounting cannot share a portal account database".into());
        }
        let mut next: Database = store.database.clone();
        let prior = next.external.get_or_insert_with(|| Bindings {
            issuer: config.issuer.clone(),
            utc_offset_minutes: config.utc_offset_minutes,
            users: BTreeMap::new(),
        });
        if prior.issuer != config.issuer || prior.utc_offset_minutes != config.utc_offset_minutes {
            return Err("external accounting issuer or monthly boundary changed".into());
        }
        let mut ids = HashSet::new();
        for (name, id) in &prior.users {
            if crate::username::normalize(name).as_ref() != Ok(name)
                || !super::super::relay_accounting::valid_user_id(id)
                || !ids.insert(id.clone())
            {
                return Err("invalid external account binding history".into());
            }
        }
        if store
            .traffic
            .external_subjects()
            .any(|id| !ids.contains(id))
        {
            return Err("traffic history is missing its original external identity binding".into());
        }
        let mut changed = store.database.external.is_none();
        for (name, id) in &config.users {
            match prior.users.get(name) {
                Some(existing) if existing == id => {}
                None if ids.insert(id.clone()) => {
                    prior.users.insert(name.clone(), id.clone());
                    changed = true;
                }
                _ => return Err("external account cannot reuse a bound name or user ID".into()),
            }
        }
        if prior.users.len() > MAX_ACCOUNTS {
            return Err("external account binding capacity exceeded".into());
        }
        let paid = store.traffic.paid_subjects().cloned().collect();
        let allowances = MonthlyTrafficAllowances {
            free_user_bytes: config.free_monthly_bytes,
            paid_user_bytes: config.paid_monthly_bytes,
        };
        store
            .traffic
            .check_external_limits(allowances.free_user_bytes.max(allowances.paid_user_bytes))
            .map_err(|_| "configured allowance overflows existing paid traffic")?;
        if changed {
            store
                .save(next)
                .map_err(|_| "cannot persist external identity binding")?;
        }
        Ok(Self {
            store,
            paid,
            offset: config.utc_offset_minutes,
            allowances,
        })
    }

    pub(in crate::portal) fn has_recharged(&self, id: &str) -> bool {
        self.paid.contains(id)
    }

    pub(in crate::portal) fn snapshot(&self, id: &str) -> Result<TrafficQuota, Problem> {
        let period = traffic_period_at(crate::management::now(), self.offset);
        self.store.traffic.snapshot(
            id,
            &period,
            self.offset,
            self.allowances.budget(self.paid.contains(id), false),
        )
    }

    pub(in crate::portal) fn reserve(
        &mut self,
        id: &str,
        frame: TrafficFrame,
    ) -> Result<(), Problem> {
        let period = traffic_period_at(crate::management::now(), self.offset);
        self.store.traffic.reserve_frame(
            &self.store.directory,
            id,
            &period,
            self.offset,
            frame,
            self.allowances.budget(self.paid.contains(id), false),
        )
    }

    pub(in crate::portal) fn grant(
        &mut self,
        id: &str,
        period: &str,
        order: &str,
        bytes: u64,
        active: bool,
    ) -> Result<bool, Problem> {
        if !active && !self.store.traffic.has_grant(order) {
            return Err(Problem::SessionNotFound);
        }
        let current = traffic_period_at(crate::management::now(), self.offset);
        let free = self
            .allowances
            .free_user_bytes
            .max(self.allowances.paid_user_bytes);
        self.store
            .traffic
            .check_external_grant(id, period, order, bytes, free)?;
        let inserted =
            self.store
                .traffic
                .grant(&self.store.directory, id, period, &current, order, bytes)?;
        // A successful, fsynced external order is the proof of the paid tier. On
        // restart reconstruct this cache from the original grants in the journal.
        self.paid.insert(id.to_owned());
        Ok(inserted)
    }
}
