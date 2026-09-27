//! The wallet registry. Wallets are assigned by hand; there is no selection here and
//! there can be none.

use crate::Mode;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use sqlx::postgres::PgPool;

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Wallet {
    pub address: String,
    pub nickname: Option<String>,
    pub mode: Mode,
    pub stake_usd: Decimal,
    pub max_slippage_pct: Decimal,
    pub enabled: bool,
    pub created_at: DateTime<Utc>,
}

impl Wallet {
    /// The display name: the nickname, otherwise a shortened address.
    pub fn display(&self) -> String {
        match &self.nickname {
            Some(n) if !n.is_empty() => n.clone(),
            _ => {
                let a = &self.address;
                if a.len() > 10 {
                    format!("{}…{}", &a[..6], &a[a.len() - 4..])
                } else {
                    a.clone()
                }
            }
        }
    }
}

pub struct WalletRepo<'a> {
    pool: &'a PgPool,
}

impl<'a> WalletRepo<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self { pool }
    }

    /// A new wallet is always born in shadow and disabled: it starts trading real money
    /// only after an explicit decision by the operator.
    pub async fn add(&self, address: &str, nickname: Option<&str>) -> anyhow::Result<Wallet> {
        let w = sqlx::query_as::<_, Wallet>(
            "INSERT INTO wallets (address, nickname) VALUES ($1, $2) RETURNING *",
        )
        .bind(address)
        .bind(nickname)
        .fetch_one(self.pool)
        .await?;
        Ok(w)
    }

    pub async fn get(&self, address: &str) -> anyhow::Result<Option<Wallet>> {
        let w = sqlx::query_as::<_, Wallet>("SELECT * FROM wallets WHERE address = $1")
            .bind(address)
            .fetch_optional(self.pool)
            .await?;
        Ok(w)
    }

    pub async fn list(&self) -> anyhow::Result<Vec<Wallet>> {
        let ws = sqlx::query_as::<_, Wallet>("SELECT * FROM wallets ORDER BY created_at")
            .fetch_all(self.pool)
            .await?;
        Ok(ws)
    }

    pub async fn set_mode(&self, address: &str, mode: Mode) -> anyhow::Result<()> {
        self.update_audited(address, "mode", "mode::text", "$2::mode", mode.as_str())
            .await
    }

    pub async fn set_stake(&self, address: &str, stake_usd: Decimal) -> anyhow::Result<()> {
        self.update_audited(
            address,
            "stake_usd",
            "stake_usd::text",
            "$2::numeric",
            &stake_usd.to_string(),
        )
        .await
    }

    pub async fn set_slippage(&self, address: &str, pct: Decimal) -> anyhow::Result<()> {
        self.update_audited(
            address,
            "max_slippage_pct",
            "max_slippage_pct::text",
            "$2::numeric",
            &pct.to_string(),
        )
        .await
    }

    /// The nickname is the only thing the dashboard is allowed to change. The actor comes
    /// from outside: "operator" does not answer who exactly did the renaming.
    pub async fn set_nickname(
        &self,
        address: &str,
        nickname: &str,
        actor: &str,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        let old: Option<String> =
            sqlx::query_scalar("SELECT nickname FROM wallets WHERE address = $1 FOR UPDATE")
                .bind(address)
                .fetch_one(&mut *tx)
                .await?;
        sqlx::query("UPDATE wallets SET nickname = $2 WHERE address = $1")
            .bind(address)
            .bind(nickname)
            .execute(&mut *tx)
            .await?;
        sqlx::query(
            "INSERT INTO wallet_events (wallet, field, old_value, new_value, actor)
             VALUES ($1, 'nickname', $2, $3, $4)",
        )
        .bind(address)
        .bind(old.unwrap_or_default())
        .bind(nickname)
        .bind(actor)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn set_enabled(&self, address: &str, enabled: bool) -> anyhow::Result<()> {
        self.update_audited(
            address,
            "enabled",
            "enabled::text",
            "$2::boolean",
            &enabled.to_string(),
        )
        .await
    }

    /// One transaction per change: the old value is read, the new one is written, a trace
    /// remains. A wallet's settings must not be changed without a trace — otherwise there
    /// is no honest way, afterwards, to answer why it traded the way it did.
    async fn update_audited(
        &self,
        address: &str,
        field: &str,
        read_expr: &str,
        write_expr: &str,
        new_value: &str,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;

        let old: String = sqlx::query_scalar(&format!(
            "SELECT {read_expr} FROM wallets WHERE address = $1 FOR UPDATE"
        ))
        .bind(address)
        .fetch_one(&mut *tx)
        .await?;

        sqlx::query(&format!(
            "UPDATE wallets SET {field} = {write_expr} WHERE address = $1"
        ))
        .bind(address)
        .bind(new_value)
        .execute(&mut *tx)
        .await?;

        sqlx::query(
            "INSERT INTO wallet_events (wallet, field, old_value, new_value, actor)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(address)
        .bind(field)
        .bind(old)
        .bind(new_value)
        .bind("operator")
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }
}
