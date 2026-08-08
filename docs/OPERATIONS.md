# Bank-SMS module — operations

Everything below concerns the `banksms` feature inside `apex-rust`. The service
continues to serve its existing routes unchanged.

## Configuration

| Variable | Default | Notes |
|---|---|---|
| `DATABASE_URL` | — | required |
| `JWT_SECRET` | — | **required**; must equal FalconGo's. The service now refuses to boot without it (it used to fall back to the literal string `"secret"`, silently accepting forged admin tokens). |
| `SERVER_PORT` | `8080` | prod uses `3002` |
| `WHATSAPP_API_URL` | `http://127.0.0.1:3000` | |
| `WHATSAPP_API_TOKEN` | unset | reserved; the WhatsApp API has no auth today |
| `TARGET_CHAT_JID` | unset | `201280701070@s.whatsapp.net`. Ingest is disabled if empty. |
| `POLL_INTERVAL_SECS` | `60` | |
| `OVERLAP_WINDOW_SECS` | `300` | how far back each poll re-reads |
| `POLL_BACKOFF_MAX_SECS` | `900` | backoff ceiling |
| `INGEST_ENABLED` | `true` | set `false` to run API-only |

## Migrations

Applied automatically on boot, recorded in **`banksms._sqlx_migrations`** — not
the default `_sqlx_migrations`, which apex-petroapp owns in this same database.

**Never edit a migration that has been applied.** sqlx checksums them; an edited
file fails at boot with `VersionMismatch`.

## Metrics

`GET /metrics` — Prometheus text format, unauthenticated, loopback-only. Contains
counters only: no message bodies, no amounts.

The categories are deliberately separate:

| Metric | Meaning | Alert on it? |
|---|---|---|
| `banksms_messages_ignored_total` | Chat noise rejected by triage | **No.** ~93% of messages in the target chat are ordinary conversation. This number is *supposed* to be large. |
| `banksms_messages_partial_total` | Looked like a bank SMS, parsed incompletely | Watch |
| `banksms_messages_unmatched_total` | Looked like a bank SMS, did not parse | Watch |
| `banksms_unmatched_skeletons` | Recurring formats with no template | **Yes — this is the alarm.** |
| `banksms_poll_errors_total` | Failed poll cycles | Alert if sustained |
| `banksms_auth_failures_total` | Rejected tokens | Watch for spikes |

### The alarm condition

> A `skeleton_hash` recurring **3 or more times** with no matching template means
> a bank changed its SMS format, and every message in that format is losing data.

It is logged at `ERROR` with an example body, so the new template can be written
straight from the log line, and surfaced at `GET /api/v1/admin/skeletons`.

A **one-off** unmatched message is almost always a human message that slipped past
the triage gate and must **not** page anyone. Caveat worth remembering: the
`cib_card` format appeared exactly once in seven months of history and was a real
bank template. Recurrence is a strong prior, not proof — one-offs still land in
the review queue, they just do not raise an alarm.

## Triage thresholds

`DEFAULT_THRESHOLD = 40`, chosen from a sweep over 479 real messages:

```
bank SMS  n=34   score min 100
chatter   n=445  score p50 0, p95 5, max 20
```

Every threshold from 30 to 80 gives zero bank SMS lost and zero chatter queued;
40 sits mid-band. Re-run the sweep after any change to the signal weights:

```bash
cargo test threshold_sweep -- --ignored --nocapture
```

Audit what the gate rejected at `GET /api/v1/raw/ignored`, ordered by score
descending — the near-misses are at the top.

## The WhatsApp API

Unauthenticated by design and bound to `127.0.0.1:3000` via an explicit
`--host 127.0.0.1` **CLI flag**, not a config file. Anyone restarting it without
that flag exposes it publicly. Verify after any restart:

```bash
ss -tlnp | grep :3000
```

It must show `127.0.0.1:3000`, never `0.0.0.0:3000`. It runs as a bare host
process, not in Docker, so the `-p 3000:3000` iptables-ahead-of-ufw trap does not
apply here — but it would if it were ever containerised.

This service exposes **no** endpoint that proxies or forwards an arbitrary path to
the WhatsApp API, and `ingest::whatsapp_client` is the only module that talks to
it.

## Backfill

```bash
apex-rust backfill
```

Walks chat history through the same insert path the poller uses, then exits
without starting the HTTP server. Deliberately does **not** touch the cursor: the
cursor tracks the live tail, and moving it from a historical walk would make the
poller skip the present.

## Cutting the dashboard over, and dropping `fleet_expenses`

`migrations_manual/20260808120300_drop_fleet_expenses.{up,down}.sql` is **not** in
the automatic chain, on purpose.

`apex-rust` still serves `/api/v1/fleet-expenses` as a union over
`fleet_expenses` + `fuel_events` + `loans`, and `src/db/expense_queries.rs`
references `fleet_expenses` in ten places. Applying the drop on a routine deploy
would break that endpoint the moment the service restarted, with no warning.

The sequence:

1. Deploy. The copy migration runs; `banksms.transactions` gains 223 imported
   rows. `public.fleet_expenses` is untouched and the old endpoint keeps working.
2. Move `apex-react` onto `/api/v1/transactions?source=import`.
3. Repoint or retire `list_unified_expenses_handler`. Note `fuel_events` and
   `loans` are **not** migrated and still belong to `public`.
4. Only then apply the drop by hand:

   ```bash
   psql -d apex -f migrations_manual/20260808120300_drop_fleet_expenses.up.sql
   ```

The drop **renames** rather than destroys (`fleet_expenses_archived_20260808`), so
a missed reference fails loudly instead of silently returning an empty result set,
and the `.down.sql` can restore it. It also refuses to run at all unless the row
count and the money total match what is in `banksms.transactions` — verified by
deliberately deleting one row and watching it abort.

Drop the archived table by hand once you are satisfied.

### Do NOT drop `public.expenses` or `public.loans`

Different tables, different owner. Both are GORM-`AutoMigrate`d by FalconGo
(`Models/setup.go`), `expenses` backs four live FalconGo routes plus payslip
calculations, and FalconGo would recreate either one empty on its next restart.
Dropping them requires modifying FalconGo, which is out of scope.

## Deployment

`/etc/systemd/system/apex-rust.service` — pin the WhatsApp bind flag in *its*
unit too, not just this one.

```ini
[Unit]
Description=apex-rust
After=network.target postgresql.service

[Service]
Type=simple
WorkingDirectory=/opt/apex-rust
EnvironmentFile=/opt/apex-rust/.env
ExecStart=/opt/apex-rust/apex-rust
Restart=always
RestartSec=5
# The service binds 127.0.0.1 in code; nginx terminates TLS in front of it.
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ReadWritePaths=/opt/apex-rust/logs

[Install]
WantedBy=multi-user.target
```

## Logging

`env_logger`, matching the rest of apex-rust rather than the `tracing` JSON setup
the original spec assumed — switching a live service's logging was not worth the
churn. Set `RUST_LOG=info`. The alarm is the only `ERROR`-level line emitted in
normal operation.
