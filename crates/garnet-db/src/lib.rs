//! Access to the Garnet database. This crate knows about SQL and nothing about trading.

use sqlx::postgres::{PgPool, PgPoolOptions};

pub use garnet_types::{Mode, Side};

pub mod repos;
pub mod testing;

pub use repos::actions::{ActionRepo, LeaderActionRow, NewLeaderAction};
pub use repos::controls::{ControlRepo, StoredIntent};
pub use repos::equity::{EquityRepo, EquitySnapshot};
pub use repos::markets::{Market, MarketRepo, NewMarket};
pub use repos::matchup::{MatchupRepo, MatchupReport, MatchupRow, MatchupStatus};
pub use repos::positions::{Position, PositionRepo};
pub use repos::reports::{
    FireDepth, LatencyStage, ModePnl, OpenPosition, ReportRepo, SignalRow, SlippageBucket,
    SourceRace,
};
pub use repos::settlements::{Settlement, SettlementRepo};
pub use repos::signals::{Order, Signal, SignalRepo};
pub use repos::trades::{LeaderTrade, NewLeaderTrade, Seen, Source, TradeRepo};
pub use repos::wallets::{Wallet, WalletRepo};

#[derive(Clone)]
pub struct Db {
    pool: PgPool,
}

impl Db {
    pub async fn connect(url: &str) -> anyhow::Result<Self> {
        let pool = PgPoolOptions::new().max_connections(5).connect(url).await?;
        Ok(Self { pool })
    }

    pub async fn migrate(&self) -> anyhow::Result<()> {
        sqlx::migrate!("../../migrations").run(&self.pool).await?;
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn actions(&self) -> ActionRepo<'_> {
        ActionRepo::new(&self.pool)
    }

    pub fn controls(&self) -> ControlRepo<'_> {
        ControlRepo::new(&self.pool)
    }

    pub fn reports(&self) -> ReportRepo<'_> {
        ReportRepo::new(&self.pool)
    }

    pub fn wallets(&self) -> WalletRepo<'_> {
        WalletRepo::new(&self.pool)
    }

    pub fn trades(&self) -> TradeRepo<'_> {
        TradeRepo::new(&self.pool)
    }

    pub fn positions(&self) -> PositionRepo<'_> {
        PositionRepo::new(&self.pool)
    }

    pub fn settlements(&self) -> SettlementRepo<'_> {
        SettlementRepo::new(&self.pool)
    }

    pub fn equity(&self) -> EquityRepo<'_> {
        EquityRepo::new(&self.pool)
    }

    pub fn signals(&self) -> SignalRepo<'_> {
        SignalRepo::new(&self.pool)
    }

    pub fn markets(&self) -> MarketRepo<'_> {
        MarketRepo::new(&self.pool)
    }

    pub fn matchup(&self) -> MatchupRepo<'_> {
        MatchupRepo::new(&self.pool)
    }
}
