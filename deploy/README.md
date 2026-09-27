# Deployment

All three processes run from `~/garnet-polymarket` as the `deploy` user and read the same
`.env`. The infrastructure (Postgres, Redis, NATS) lives in docker compose with
`restart: unless-stopped`; the units do not bring it up, they only wait for
Docker.

## Install (requires sudo)

```bash
sudo cp ~/garnet-polymarket/deploy/garnet-core.service /etc/systemd/system/
sudo cp ~/garnet-polymarket/deploy/garnet-tg.service   /etc/systemd/system/
sudo cp ~/garnet-polymarket/deploy/garnet-dash.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now garnet-core garnet-tg garnet-dash
```

The watcher is a timer, not a service, and is optional but recommended:

```bash
sudo cp ~/garnet-polymarket/deploy/garnet-watch.service /etc/systemd/system/
sudo cp ~/garnet-polymarket/deploy/garnet-watch.timer   /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now garnet-watch.timer
```

## Verify

```bash
systemctl status garnet-core garnet-tg garnet-dash --no-pager
journalctl -u garnet-core -n 50 --no-pager
journalctl -u garnet-tg   -n 20 --no-pager
```

Expected in the core's log: the `PolyGnosisSafe` signature type, the number of
wallets under observation, the free collateral, and a `health: …` line. In the
bot's log: `bot started, chats allowed: N` and `pushes: nats://…`.

## Upgrade after a build

```bash
cd ~/garnet-polymarket && ./deploy/deploy.sh
sudo systemctl restart garnet-core garnet-tg garnet-dash
```

`.env` is read **at startup**: after changing a variable, a `restart` is
required.

## Stopping live trading

The quick way is `/kill` in Telegram: it writes to `controls.manual_stop`, and
the trading process synchronises with that row on every health tick. The stop
survives a process restart and is lifted with `/resume`.

`systemctl stop garnet-core` also stops trading, but it stops shadow along with
it — and shadow is a measuring instrument whose data there is no reason to lose.

## Backups

```bash
docker exec garnet-postgres-1 pg_dump -U garnet -d garnet \
  | gzip > ~/backups/garnet-$(date +%Y%m%d-%H%M).sql.gz
```

The Postgres data lives in the named volume `garnet_pgdata`. Both the project
name and the volume name are pinned in the compose files, so neither is derived
from the directory the repository sits in. Tests go to a **separate** database,
`garnet_test`: the default in the code points there, so a forgotten environment
variable sends a test run somewhere there is nothing to break.

Coming from an installation where those names were not pinned, the volume was
called `<directory>_garnet_pgdata`. Move the data across once, with the stack
stopped:

```bash
docker compose down
docker run --rm -v <directory>_garnet_pgdata:/from -v garnet_pgdata:/to \
  alpine sh -c 'cd /from && cp -a . /to'
docker compose up -d
```

Verify the copy before removing the old volume: it holds the settlement
history, and there is no second copy of it.

Note that a dump contains the addresses of the wallets you copy. Keep backups
off version control and off shared storage — `.gitignore` covers `*.sql.gz`, but
only inside this repository.

## Dashboard

`garnet-dash` listens on `127.0.0.1:3001` and is exposed by Caddy. A minimal
`/etc/caddy/Caddyfile` block:

```
dashboard.example.com {
    reverse_proxy 127.0.0.1:3001
}
```

There are no browser login endpoints in Garnet at all, so nothing needs to be
blocked in front of it — the door does not exist rather than being locked.

Access is through the Telegram mini-app only: every request carries `initData`,
the signature is verified with the bot token, and the id is checked against the
same owner list the bot uses. It opens from the `/app` button.
