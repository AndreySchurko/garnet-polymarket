//! The fork: a live executor or an explicit refusal.
//!
//! `ClobExec` is declared with an `async fn` in the trait, so a `dyn` object cannot be built
//! from it, and the choice has to be made by type. Hence an enum rather than a box.
//!
//! The main thing here is what the branch without keys does. It **refuses** rather than
//! sending a live wallet into shadow: a mode swapped behind the operator's back paints them
//! a profit that is not in the account.

use crate::clob_adapter::ClobAdapter;
use garnet_clob::traits::ClobClient;
use garnet_clob::GarnetClobClient;
use garnet_core::execute::{ClobExec, ExecError, OrderRequest};
use garnet_core::shadow::Fill;

pub enum Exec<C: ClobClient = GarnetClobClient> {
    /// The keys are set: the order goes to the exchange.
    Live(ClobAdapter<C>),
    /// There are no keys: every live order gets a refusal with a reason.
    Off,
}

impl<C: ClobClient> Exec<C> {
    pub fn live(client: C) -> Self {
        Self::Live(ClobAdapter::new(client))
    }

    #[must_use]
    pub fn off() -> Self {
        Self::Off
    }

    /// The client under the adapter — for readiness checks and tests.
    pub fn client(&self) -> Option<&C> {
        match self {
            Self::Live(adapter) => Some(adapter.client()),
            Self::Off => None,
        }
    }

    #[must_use]
    pub fn is_live(&self) -> bool {
        matches!(self, Self::Live(_))
    }
}

impl<C: ClobClient + Sync> ClobExec for Exec<C> {
    async fn place_ioc(&self, req: &OrderRequest) -> Result<Fill, ExecError> {
        match self {
            Self::Live(adapter) => adapter.place_ioc(req).await,
            // A refusal before the exchange: it touched no money, a retry is safe.
            Self::Off => Err(ExecError::Rejected(
                "live execution is disabled: no keys are set".to_string(),
            )),
        }
    }
}
