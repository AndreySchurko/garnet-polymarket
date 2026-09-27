//! The long-polling loop.
//!
//! Two rules that are easy to lose:
//!
//! * **the offset always advances**, even when an update is discarded — otherwise the
//!   same foreign update arrives forever and the loop never moves on;
//! * **not a single request goes to a chat that is not ours**: silence, not a refusal.

use crate::api::Api;
use crate::commands::handle;
use crate::guard::allowed;
use crate::update::{parse_update, Incoming};
use garnet_db::Db;

/// One pass: fetch the updates, execute, reply. Returns how many updates were read.
///
/// # Errors
///
/// A `getUpdates` failure. An error handling one update does not bring down the loop: it
/// is printed and processing continues — one malformed update must not deprive the
/// operator of the controls.
pub async fn poll_once(
    api: &Api,
    db: &Db,
    whitelist: &[i64],
    offset: &mut i64,
    timeout_secs: u64,
) -> anyhow::Result<usize> {
    let updates = api.get_updates(*offset, timeout_secs).await?;
    let count = updates.len();

    for raw in updates {
        if let Some(id) = raw["update_id"].as_i64() {
            *offset = id + 1;
        }
        let Some(incoming) = parse_update(&raw) else {
            continue;
        };
        if !allowed(whitelist, chat_of(&incoming)) {
            continue;
        }
        if let Err(e) = serve(api, db, &incoming).await {
            eprintln!("update not handled: {e}");
        }
    }
    Ok(count)
}

fn chat_of(incoming: &Incoming) -> i64 {
    match incoming {
        Incoming::Message { chat_id, .. } | Incoming::Button { chat_id, .. } => *chat_id,
    }
}

async fn serve(api: &Api, db: &Db, incoming: &Incoming) -> anyhow::Result<()> {
    // The press is acknowledged first: while we go to the database, the client shows a
    // spinner.
    if let Incoming::Button { query_id, .. } = incoming {
        api.answer_callback(query_id).await?;
    }

    let Some(reply) = handle(db, incoming).await? else {
        return Ok(());
    };

    match incoming {
        Incoming::Button {
            chat_id,
            message_id,
            ..
        } if reply.edit => api.edit_message(*chat_id, *message_id, &reply.text).await,
        Incoming::Button { chat_id, .. } | Incoming::Message { chat_id, .. } => {
            api.send_message(*chat_id, &reply.text, &reply.buttons)
                .await
        }
    }
}
