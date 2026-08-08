# RECON — Phase 0 findings

**Date:** 2026-08-08 · **Target VPS:** `187.124.33.153` (`srv1460366`, Debian 13, kernel 6.12.88)
**Scope (revised per your instruction):** **extend `apex-rust`** — no new service. Includes
deprecating the old expenses schema, migrating its data into the new `banksms` tables, and dropping
the old tables afterwards.
**Status:** COMPLETE. All six phases built, verified and committed. This document
is kept as the record of what production actually looked like on 2026-08-08 and
where the original spec disagreed with it.

**How the §9 decisions were resolved** (the owner delegated them with "do what you
deem best"):

| # | Decision taken |
|---|---|
| A Stack | Kept actix-web + `env_logger`; added `rust_decimal`, `regex`, `reqwest`, `chrono-tz`, `errors.rs`, `sqlx::migrate!`. Upgraded sqlx 0.7 -> 0.8 (cargo flags 0.7.4 as containing code a future rustc rejects). Compiled with no changes to existing code. |
| B Scope | `fleet_expenses` only: copied, verified, and a drop migration written but kept OUT of the automatic chain. `fuel_events`/`loans` still read from `public`. `expenses`/`loans` NOT dropped -- FalconGo's GORM recreates them. |
| C Columns | Added `payment_method`, `company`, `car_no_plate`, `paid_by`, `description`, `import_source_id`. |
| D Overrides | **(b) `transaction_overrides` table.** `GET /transactions/:id/history` needs a real audit trail; (a) cannot serve it and (c) was unjustified. |
| E Amounts | Row producer writes `parsed_*` + `parse_method`; overrides layer on top. Safe because every reparse path is scoped to `source = 'whatsapp'`. |
| F Parser | 34/34 tier-1 target, patterns stored post-normalization, boot-time assertion that every seeded pattern matches real traffic. Triage threshold 40, chosen from a sweep. |

**Migration-table isolation:** the fix proposed below (`set_migration_table`) does
NOT exist in sqlx 0.8 -- the name is hardcoded. Solved instead by running the
migrator over one dedicated connection whose `search_path` starts at `banksms`.


> Lives in the FalconGo repo for review convenience; moves to `apex-rust/docs/RECON.md` on approval.

---

## 1. FalconGo's JWT

**Source:** [utils/jwt.go](utils/jwt.go), [Constants/Constants.go](Constants/Constants.go)

**Algorithm:** `HS256` (`jwt.SigningMethodHS256`), library `github.com/golang-jwt/jwt/v4`.
**Key source:** `JWT_SECRET` env var via `godotenv`. FalconGo `log.Fatal`s if unset.

**Exact claims struct** ([utils/jwt.go:15](utils/jwt.go:15)):

```go
type CustomClaims struct {
    UserType   string `json:"user_type"`  // "admin_user" | "driver"
    UserID     uint   `json:"user_id"`
    DriverID   uint   `json:"driver_id"`
    Permission int    `json:"permission"`
    jwt.RegisteredClaims
}
```

| Claim | Present | Wire type | Notes |
|---|---|---|---|
| `user_type` | ✅ always | string | `"admin_user"` or `"driver"` |
| `user_id` | ✅ always | **number** | `0` on driver tokens |
| `driver_id` | ✅ always | **number** | `0` on admin tokens |
| `permission` | ✅ always | **number** | `0` on driver tokens |
| `exp` | ✅ always | NumericDate | admin **31 days**, driver **365 days** |
| `iss` | ✅ always | **string** (`strconv.Itoa(userID)`) | the id as a string |
| **`sub`** | ❌ **ABSENT** | — | **spec §6 assumed this claim; it does not exist** |
| `iat` / `aud` / `nbf` | ❌ absent | — | |

### Deviations from what spec §6 assumed

1. **No `sub`.** The instruction to handle numeric-vs-string `sub` is moot: identity is **`user_id`,
   always a JSON number**. (`iss` is the same id as a string, but it's not an identity claim we should
   lean on.) Any `user_permissions` table keys on `user_id`, not `sub`.
2. **`exp` IS present** → keep `validate_exp = true`; the spec's fallback isn't needed.
3. **Never call `set_issuer()`** — `iss` varies per user, so validating it rejects every token.
4. **Never validate `aud`** — absent.
5. **Authorization vocabulary is a numeric ladder**, not roles/scopes: `permission` 1→4. apex-rust
   already gates fleet-expense writes on `Some(4)`, and `NOLON_REBUILD_PLAN.md` §3 maps 2→editor,
   3→manager, 4→org_admin. So spec §6's "map FalconGo's vocabulary to local permissions" = map an
   integer threshold, and no `banksms.user_permissions` table is needed for v1.

**Token transport:** apex-rust accepts the token from **either a `jwt` cookie or
`Authorization: Bearer`** ([auth/middleware.rs:56](/Users/shawket/Rust/apex/src/auth/middleware.rs)).
The apex-react dashboard uses the cookie. Both paths must keep working.

**Good news for Phase 3:** auth is *already built and in production* in apex-rust
(`src/auth/claims.rs`, `src/auth/middleware.rs`). Phase 3 shrinks from "build it" to "harden it":

- 🔴 **Security bug to fix:** `config.rs` does
  `env::var("JWT_SECRET").unwrap_or_else(|_| "secret".to_string())`.
  If `JWT_SECRET` is ever unset, apex-rust **silently boots with the secret `"secret"`** and will
  accept trivially forged admin tokens. FalconGo refuses to start in that case; apex-rust degrades
  quietly. Should become a hard `.expect()`.
- ⚠️ `Validation::default()` is used, which *happens* to be HS256. Spec §6 requires pinning
  `Validation::new(Algorithm::HS256)` explicitly. Cheap fix, worth doing.
- ⚠️ No clock-skew leeway configured (spec asks for ~60s).
- ⚠️ Handlers receive `Claims` directly. Spec §6 wants an `AuthContext { user_id, permissions }` so a
  future FalconGo change is isolated to one file. Worth introducing as the new endpoints land.

---

## 2. WhatsApp Go API

**Product:** `aldinokemal/go-whatsapp-web-multidevice` · **Version: 9.0.0**
(`whatsapp_9.0.0_linux_amd64.zip`, binary dated 2026-07-19; `v8.3.5` kept as `.bak`)
**Process:** `/var/www/WhatsappService/linux-amd64 --host 127.0.0.1 rest` (pid 1991383)
**Auth:** none, as expected.

### ✅ Security check (spec §9) — PASSES

```
LISTEN 0 4096 127.0.0.1:3000 0.0.0.0:* users:(("linux-amd64",pid=1991383,fd=12))
```

Bound to **`127.0.0.1:3000`**. It is a **bare host process, not Docker**, so the
`-p 3000:3000` / iptables-ahead-of-ufw trap in spec §9 does not apply.
⚠️ The bind is enforced by a **CLI flag** (`--host 127.0.0.1`), not a config file — anyone restarting
it without the flag silently exposes it. Recommend pinning the flag in a systemd unit.

### There IS a history endpoint — the §5 ingestion design is valid

**`GET /chats?limit=&offset=&search=`** — `limit` capped at **100**; 1140 chats total; `search=`
filters on chat name server-side.

**`GET /chat/{chat_jid}/messages?limit=&offset=&start_time=&search=`**

| Param | Supported | Evidence |
|---|---|---|
| `limit` / `offset` | ✅ | |
| **`start_time`** | ✅ **HONORED** | `start_time=2027-01-01T00:00:00Z` → `returned: 0` (of `total: 5002`) |
| `search` | ✅ | `search=EGP` on a non-bank chat → `returned: 0` |
| `end_time` | assumed symmetric | not exercised |

**`start_time` is the most important finding for §5.** The poller doesn't need blind offset paging —
it can request `start_time = last_wa_timestamp − OVERLAP_WINDOW` directly. That is exactly the
composite-cursor design the spec asks for, and it makes the generous 5-minute overlap genuinely free.

⚠️ `pagination.total` is the **unfiltered** total, not the filtered count. Don't use it to detect the
last page — page until `data` is empty.

### Message response shape (verbatim)

```json
{ "id": "AC4B043147DEB40D107F510436F50E35", "chat_jid": "…@g.us",
  "sender_jid": "201285187927@s.whatsapp.net", "content": "Transfer reference #ac8fc63f …",
  "timestamp": "2026-08-08T11:28:33Z", "is_from_me": false,
  "media_type": "image", "filename": "…", "url": "…", "file_length": 114714,
  "created_at": "…", "updated_at": "…" }
```

| `raw_messages` column | API field | Notes |
|---|---|---|
| `wa_message_id` | `id` | 32-char uppercase hex. Dedup key. |
| `chat_jid` / `sender` / `is_from_me` | `chat_jid` / `sender_jid` / `is_from_me` | |
| `wa_timestamp` | `timestamp` | **RFC 3339, always `Z`/UTC** — no conversion on ingest |
| `body` | `content` | **`""` for media messages** |

**Timezone:** the envelope is UTC; times *inside* the SMS body are **Africa/Cairo**. Verified on 4
independent messages (envelope `11:32:20Z` ↔ body `at 14:32` ⇒ UTC+3 in Aug 2026). Egypt observes
DST, so **use the `Africa/Cairo` tz database, never a fixed offset**.

**Media messages carry `content: ""`** — 250 of 729 in the target chat. Store raw-first, route to
`ignored` on empty body; not errors. Plain text has `media_type: ""`, not `"text"`.

**JIDs:** groups `<digits>@g.us`, individuals `<msisdn>@s.whatsapp.net`. Connected device:
`201061856523@s.whatsapp.net`.

### 🔴 The target chat is a 1:1 DM shared with a human — both §7.1 discriminators are UNAVAILABLE

**`TARGET_CHAT_JID = 201280701070@s.whatsapp.net`** (display name "Shawket Ibrahim", 729 messages,
2026-01-13 → 2026-08-08). Found by scanning the 300 most-recently-active chats for `search=IPN`.
No chat is named for a bank; the SMS land in an **ordinary one-to-one conversation**.

Spec §7.1 offers two discriminators "both stronger than content scoring" and asks me to check
availability. **Neither exists:**

1. **No forwarder prefix/wrapper.** The SMS arrive as bare text, no automated-forwarder envelope.
2. **`is_from_me` / sender JID cannot discriminate** — bank SMS and the human's own chat messages
   share the same JID and `is_from_me: false`:

   ```
   [11:32:20Z] fromme=False sender=201280701070@…  'Transfer reference #ac8fc63f of EGP 85.00 …'
   [09:02:42Z] fromme=False sender=201280701070@…  'هو لازم contact'
   ```

   `is_from_me: true` (498 of 729) marks *our own* replies — all chatter.

**Consequence:** content scoring is the **only** mechanism, not a backstop. The triage gate becomes
load-bearing, which raises the stakes on the §7.1 thresholds and makes the §7.3 reclassify endpoints
mandatory rather than nice-to-have.

---

## 3. Parser: seed templates validated against 729 real messages

Corpus exported to `bank_chat_corpus.json`; classified with the §7.0 normalization pre-pass, then the
three §7.2 seed patterns.

| Bucket | Count | Share of text messages |
|---|---|---|
| Text messages (non-empty `content`) | **479** | 100% |
| ├ `ref_balance` (19555) | 12 | 2.5% |
| ├ `abk` (19322) | 11 | 2.3% |
| ├ `arabic_ipn` (19666) | 10 | 2.1% |
| ├ **bank-ish, no template match** | **2** | 0.4% |
| └ ordinary human chatter | **444** | **92.7%** |
| Media / empty-body | 250 | (excluded above) |

**All three seed templates match real traffic** — 33 messages via tier 1. Only **7%** of this chat is
bank SMS.

### ⚠️ The "23/23" acceptance target is stale

Spec §7/§10 say "23/23 parsed". The live corpus has **33** matching the seed templates plus **1**
genuinely new format. Propose the criterion becomes **34/34 via tier 1** against fixtures cut from
this corpus, with the 444 chatter messages required to route to `ignored`. Confirm before I cut
fixtures (§9-E).

### ⚠️ A 4th template is needed — CIB direct-debit card withdrawal (not in the spec)

```
تم سحب مبلغ  EGP 8000.00  من بطاقة الخصم المباشر المنتهية بـ **2234
من CIB EL NASRBR 2 BNA في 08/08/26 10:32 ، الرصيد المتاح EGP 8579.76
```

Structurally distinct from all three seed templates: a **card** mask (`**2234`) not an account mask;
a **merchant/branch** (`CIB EL NASRBR 2 BNA`) instead of a counterparty; **no reference number at
all**; `سحب` (withdrawal) not `خصم`; date format **`%d/%m/%y`** — a *fourth* distinct format; trailing
balance. Seen once.

- Direction map needs `سحب → out`.
- `parsed_reference` must tolerate NULL on a **tier-1, confidence-100** row — the field genuinely
  doesn't exist in the source. The spec implies template matches are complete; this one isn't.
- The date-format unit test grows to **four** assertions.
- Per §7.2 tier 3 "seen once ≈ human message" — this is a counter-example. The heuristic is
  imperfect; frequency alone shouldn't be the only routing signal.

### 🔴 Normalization mutates the spec's own Arabic patterns — they cannot match as written

Spec §7.0 mandates (for matching) `أ إ آ ٱ → ا`, `ى → ي`, `ة → ه`. Spec §7.2 then supplies the
`arabic_ipn` pattern containing **`إلى`**, and real messages contain `أنه`, `اللحظية`, `الإنترنت`.
After normalization the haystack holds `الي` — so the literal `إلى` in the pattern **can never match**.
The printed patterns are written against **raw** text while normalization is specified to run
**before** matching. The two sections contradict each other.

**Resolution used here (and proposed for the seed data):** store patterns in **post-normalization**
form — `إلى`→`الي`, `إضافة`→`اضافه`. With that change all three match real traffic (numbers above).
Recommend a **boot-time assertion that every enabled pattern matches ≥1 fixture**, so this class of
bug fails loudly instead of silently draining into the review queue.

### Other confirmed parser facts

- **Both directions occur:** 32 debits, **1 credit** (`abk`/`credited`). The inbound path is real.
- **4 accounts, 3 mask styles:** `6001-01` (12), `*7647` (11), `********9276` (7), `********5447` (3).
- **Currency `EGP` in 33/33.** No multi-currency traffic yet.
- **All four date formats verified against envelope timestamps:** `abk` `8/7/26`=`%m/%d/%y`;
  `arabic_ipn` `08-08-2026`=`%d-%m-%Y`; `ref_balance` `08/08/2026`=`%d/%m/%Y`;
  `cib_card` `08/08/26`=`%d/%m/%y`. Confirms the spec's warning that a shared date parser would
  silently produce wrong dates.
- **Fee-inclusive amounts are real and mixed.** Observed `2002.00`, `1001.0`, `8008.0`, `2702.7`,
  `1201.2`, `500.5`, `9009.0` (all exactly principal + 0.1%) alongside clean `85.00`, `4985.00`,
  `15000.00`. Both conventions coexist **in the same chat** — confirms "derived, not stored".
- **🔴 A transaction keyword appears in human messages.** `تعديل لخصم قطس كاوتش` ("adjustment for tire
  deduction") contains **`خصم`**, a *medium*-weight §7.1 signal. Keyword-only triage false-positives
  on it — the negative signals must carry real weight.

---

## 4. Database

**Version:** **PostgreSQL 17.10** (Debian 17.10-0+deb13u1), same VPS.
**Listeners:** `127.0.0.1:5432`, `[::1]:5432`, `172.17.0.1:5432` (docker bridge),
`100.101.100.57:5432` (Tailscale). Not public.
**Databases:** **`apex` (51 MB)** ← target, `apex_maint` (9 MB), `madar` (43 MB), `madar_demo` (16 MB).
**Login roles:** `apex`, `madar`, `postgres`, `replicator`, `shawket`. `pg_hba` grants `apex` from
`127.0.0.1/32` + two Tailscale /32s (scram-sha-256).
**Schemas in `apex`: `public` only. `banksms` is free** (`pg_namespace` count = 0). ✅

**51 tables in `apex.public`** — full list captured in the schema dump.

**Fresh dumps taken** (read-only `pg_dump`), downloaded to the session scratchpad and left at
`/tmp/` on the VPS:

| File | Size |
|---|---|
| `apex_recon_20260808.dump` (`-Fc` full) | 7.2 MB |
| `apex_schema_20260808.sql` (`-s` schema only) | 111 KB |

⚠️ They're in a **session-scoped temp dir** — tell me where to keep them permanently (§9-F).

### 🔴 `_sqlx_migrations` in `apex.public` is NOT apex-rust's — it belongs to apex-petroapp

```
 version |   description    | success |         installed_on
       1 | service tables   | t       | 2026-06-06 16:49:39+00
       2 | fuel events link | t       | 2026-06-06 16:49:39+00
       3 | seed vehicle map | t       | 2026-06-06 16:49:39+00
```

**apex-rust does not run migrations at all** — there is no `sqlx::migrate!` in `src/main.rs`, and its
`migrations/expense_table.sql` is a standalone script applied by hand.

So adding `sqlx::migrate!()` to apex-rust (spec §3 requires it) against this database would make it
**share `public._sqlx_migrations` with apex-petroapp** → version-number collisions and each service
seeing the other's rows as unknown/out-of-order.

**Required:** point apex-rust's migrator at a dedicated table, e.g.
`sqlx::migrate!().set_migration_table("banksms._sqlx_migrations")`, created inside the new schema.
This is a real landmine and cheap to avoid up front; expensive to untangle later.

---

## 5. Extending apex-rust: what the codebase already gives us, and the stack gap

**Location:** source `/Users/shawket/Rust/apex` · deployed `/opt/apex-rust` · `127.0.0.1:3002`
**Binary:** `apex-rust` · **crate:** `apex` v0.1.0, edition 2021

```
src/ main.rs config.rs
     auth/    claims.rs middleware.rs mod.rs          ← Phase 3 mostly done
     db/      expense_queries.rs session_queries.rs stats_queries.rs
     handlers/ expense.rs session.rs trip_stats.rs
     models/  expense.rs session.rs trip.rs
     utils/   msgpack.rs
migrations/ expense_table.sql                          ← not wired to sqlx::migrate!
```

### 🔴 Spec §3's stack does not match apex-rust. Extending it means adopting apex-rust's stack.

| Spec §3 requires | apex-rust reality | Resolution |
|---|---|---|
| **`axum` + `tower-http`** | **`actix-web` 4.4 + `actix-cors`** | ⚠️ **Decision §9-A** — a rewrite is not "extending" |
| `sqlx` + compile-time checked queries + `sqlx::migrate!` | `sqlx` **0.7**, no `migrate!` (see §4) | add `migrate!` w/ dedicated table; **0.7→0.8 upgrade?** (§9-A) |
| **`rust_decimal`** + macros | **absent.** `bigdecimal` feature on; money is **`f64`** | add `rust_decimal`; see §6 |
| `regex` + `OnceLock` | **absent** | add |
| `tracing` + `tracing-subscriber` JSON | **`env_logger` + `log`** | ⚠️ **Decision §9-A** |
| `figment` / `envy` config | hand-rolled `once_cell::Lazy` + `dotenv` | extend existing `Config` struct |
| `thiserror` `AppError` → RFC 7807 problem+json | `thiserror` in deps but **no `errors.rs`**; handlers return ad-hoc actix errors | add `errors.rs`; new endpoints only |
| HTTP client for the WhatsApp API | **none — no `reqwest`** | add `reqwest` |
| `jsonwebtoken` | ✅ `"9"` | keep |

**My recommendation:** keep **actix-web** and keep `log`/`env_logger`, and treat spec §3's axum/tracing
lines as superseded by "extend apex-rust". Rewriting a live service's HTTP layer and logging to
satisfy a stack preference is a large, risky change that buys nothing functional. Everything that
actually matters for correctness — `rust_decimal` for money, compile-time-checked queries, `migrate!`,
a real `AppError`, `regex`+`OnceLock` — is additive and I'd do all of it. **Confirm in §9-A.**

**Deps to add:** `reqwest` (rustls), `rust_decimal` + `rust_decimal_macros`, `regex`,
`sqlx` `rust_decimal` feature, `chrono-tz` (Africa/Cairo, DST-correct).

### Existing endpoints (all `JwtAuth`; writes require `permission >= 4`)

```
GET        /api/v1/sessions/{id}/location-pings     perm 1
GET        /api/v1/trip-statistics                  perm 3
GET  POST  /api/v1/fleet-expenses                   perm 4 on write
GET        /api/v1/fleet-expenses/statistics
GET        /api/v1/fleet-expenses/export
GET PUT DELETE /api/v1/fleet-expenses/{id}
```

⚠️ Note the existing routes are under **`/api/v1`**. Spec §8 lists bare paths (`/transactions`,
`/healthz`, …). New routes should mount under `/api/v1` for consistency — except `/healthz`/`/readyz`,
which conventionally sit at the root. Confirm in §9-E.

---

## 6. The costs endpoint and the expenses migration

You said: *"there is already a costs calculation endpoint that uses expenses table this gets dropped
and migrated to the new tables… It's data must carry over. Also it's columns to like the categories
etc."* and *"deprecating the old expenses schema and migrating its data to the new tables and also
dropping the tables after."*

**Three findings change the shape of this. §6c is a blocker.**

### 6a. There are two different "expenses" tables with different owners

| Table | Rows | What it is | Owner | Droppable? |
|---|---|---|---|---|
| **`fleet_expenses`** | **223** | The costs endpoint's table. `expense_type`, `company`, `paid_by`, `payment_method CHECK ('Cash','IPN Transfer')` | **apex-rust** (`migrate_fleet_expenses.sql`) | ✅ yes |
| `expenses` | 20 | Driver *trip* expenses — `trip_struct_id`, `driver_id` FK, `cost`, `category`, feeds payslips | **FalconGo** (GORM) | 🔴 **no — see 6c** |

Both have a category-ish column, which is why this is ambiguous: `expenses` has a literal
**`category`**, `fleet_expenses` has **`expense_type`**.

**I'm treating `fleet_expenses` as the migration target.** Decisive evidence: the costs *endpoint* is
apex-rust's `/fleet-expenses`, `/opt/apex-rust/migrate_fleet_expenses.sql` exists on prod, and the
`payment_method` CHECK is literally `'Cash' | 'IPN Transfer'` — **those IPN transfers are the bank
SMS**, hand-entered today. Confirm in §9-B.

### 6b. The endpoint is a union of THREE sources, not just `fleet_expenses`

`list_unified_expenses_handler` serves a `UnifiedExpense` over:

```rust
pub enum ExpenseSource { FleetExpense, FuelEvent, Loan }
```

…toggled by `include_fuel` / `include_loans` query flags. So the dashboard's costs view already blends
**manual fleet expenses + fuel events + driver loans**. `fuel_events` is owned by the PetroApp sync
pipeline; `loans` is driver finance. Neither belongs in `banksms.transactions`, and **`loans` has the
same FalconGo-ownership problem as `expenses`** (both are GORM-AutoMigrated — [Models/setup.go:182](Models/setup.go:182)).

**My read:** migrate **only** `fleet_expenses` into `banksms`; keep the union endpoint reading
`fuel_events` and `loans` from `public` as it does now. Confirm in §9-B.

### 6c. 🔴 `expenses` and `loans` CANNOT be dropped — FalconGo recreates them on every boot

[Models/setup.go:182-186](Models/setup.go:182):

```go
DB.AutoMigrate(
    &Expense{},   // → public.expenses
    &Loan{},      // → public.loans
    &Salary{},
)
```

`Models.Expense` ([Models/driver.go:45](Models/driver.go:45)) maps to `public.expenses`, and FalconGo
serves four live routes off it ([FiberConfig/Routes.go:274-277](FiberConfig/Routes.go:274)):
`RegisterDriverExpense`, `GetDriverExpenses`, `GetTripExpenses`, `DeleteExpense` — plus payslip
calculations in `Apis/Driver_Salary.go`.

So dropping `public.expenses` would (a) break those FalconGo endpoints and payslips, and (b) be
**futile — GORM AutoMigrate recreates the table on the next FalconGo restart**, empty. Undoing that
requires modifying FalconGo, which spec §1 constraint 1 forbids.

**`fleet_expenses` has no Go model and no GORM registration**, so it is safe to drop once the endpoint
moves. Scope the drop to `fleet_expenses` only. Confirm in §9-B.

> Note: dropping `fleet_expenses` overrides spec §3 constraint 3 ("do not modify existing tables").
> You've explicitly authorized that, so I'll proceed — recording it here as a deliberate override, and
> I'd sequence it as its own migration **after** the data is verified in `banksms` (see §9-C).

### 6d. Columns to carry over, and the data

```
fleet_expenses(
  id, car_no_plate varchar(50), expense_date date NOT NULL,
  expense_type varchar(100) NOT NULL, amount numeric(12,2) NOT NULL CHECK (amount >= 0),
  description text, company varchar(100), paid_by varchar(255),
  payment_method varchar(50) NOT NULL CHECK (IN ('Cash','IPN Transfer')),
  created_by integer NOT NULL, created_at, updated_at, deleted_at)
```

**223 rows, 0 soft-deleted.** `expense_date` **2025-11-01 → 2026-02-19**; `created_at` → **2026-02-20**.
⚠️ **Data entry stopped in February 2026** — ~6 months stale. Worth knowing before treating it as live.

`expense_type` — the "categories" (8 values):

| expense_type | rows | sum (EGP) |
|---|---|---|
| Other | 104 | 1,650,968.00 |
| Labor | 68 | 132,900.00 |
| Parts | 27 | 105,520.00 |
| Registration | 8 | 126,100.00 |
| Fuel | 6 | 204,210.00 |
| Maintenance | 4 | 23,700.00 |
| Repairs | 4 | 43,950.00 |
| Insurance | 2 | 201,250.00 |

`payment_method`: `IPN Transfer` 137 / 1,742,322.00 · `Cash` 86 / 746,276.00
`company`: NULL 176 · `Petrol Arrows` 34 · `Watanya` 8 · `TAQA` 5

**Proposed mapping into `banksms.transactions`:**

| `fleet_expenses` | `banksms.transactions` | Note |
|---|---|---|
| — | `source = 'import'` | enum already has it |
| `id` | `import_source_id` (new) | idempotent re-runs |
| `amount` | amount | ⚠️ **which column? §9-D** |
| `expense_date` | occurred_at | date-only → midnight **Africa/Cairo** → UTC |
| `expense_type` | `category` | user-owned TEXT ✅ verbatim strings |
| `description` | ⚠️ own column or a `notes` row? **§9-C** | |
| `payment_method` | **no column exists** | **new column** |
| `company` | **no column exists** | **new column** |
| `car_no_plate` | **no column exists** | **new column** |
| `paid_by` | **no column exists** | **new column** |
| `created_by` (int) | `created_by` (TEXT) | cast; it's a `users.id` |
| `deleted_at` | `deleted_at` | ✅ |

**Spec §4's schema has no home for `payment_method`, `company`, `car_no_plate`, or `paid_by`** — yet the
dashboard filters on all four (`FleetExpenseFilters`). Carrying the data over "with its columns too"
therefore means **four columns beyond spec §4**. I haven't invented them — see §9-C.

Also note: **all 223 rows have `direction = out`** (they're expenses), while `parsed_direction` on
SMS rows carries both. And `parsed_currency` has no source — default `'EGP'` for imports.

---

## 7. Dashboard

**Repo:** `Shawket4/apex-react` (public, default branch `main`, last push **2026-08-02**).
Not cloned; **not read** — outside Phase 0 as written.

Given §6, the dashboard's costs view is the real consumer contract for this migration: it decides
which columns and filters must survive, and it's the thing that breaks when `/fleet-expenses` changes
shape. **I'd recommend reading it before Phase 1 DDL is final.** Say the word and I'll clone it (§9-E).

---

## 8. Config (spec §9) — recovered values

apex-rust's existing `Config` (`src/config.rs`) has `database_url`, `jwt_secret`, `server_host`,
`server_port`, `workers`. Fields to **add**:

| Env var | Value / note |
|---|---|
| `WHATSAPP_API_URL` | `http://127.0.0.1:3000` (FalconGo hardcodes it as `Constants.WhatsappGoService`) |
| `WHATSAPP_API_TOKEN` | none today — keep `Option<String>`, unused, per spec §9 |
| `TARGET_CHAT_JID` | `201280701070@s.whatsapp.net` |
| `POLL_INTERVAL_SECS` | new |
| `OVERLAP_WINDOW_SECS` | new, default 300 |
| triage thresholds | new — **values deferred to the §7.3 stop** |

Already present: `DATABASE_URL` (`postgres://apex@127.0.0.1:5432/apex`), `JWT_SECRET`, `SERVER_HOST`,
`SERVER_PORT` (=3002 in prod; code default 8080).
**No new port needed** — this rides on apex-rust's existing `127.0.0.1:3002` behind nginx. ✅

VPS ports in use: 3000 WhatsApp, 3001 FalconGo, **3002 apex-rust**, 3003 etit-proxy, 8090
apex-petroapp, 5432 postgres, 6379 redis, 80/443/8443/8444 nginx, 5000 OSRM (Docker, published on
`0.0.0.0`).

---

## 9. STOP — decisions I need before Phase 1

**A. Stack reconciliation (§5).** Confirm: keep **actix-web** and `log`/`env_logger` (treating spec
§3's axum/`tracing` requirements as superseded by "extend apex-rust"), while adding `rust_decimal`,
`regex`, `reqwest`, `chrono-tz`, `sqlx::migrate!` (dedicated table), and an `errors.rs`.
Also: **upgrade sqlx 0.7 → 0.8?** (NOLON plan standardises on 0.8.)

**B. Migration scope (§6).** Confirm:
1. Target is **`fleet_expenses`** (223 rows), not `expenses` (20 rows).
2. **Only `fleet_expenses`** migrates; `fuel_events` and `loans` keep being read from `public`.
3. **Only `fleet_expenses` gets dropped** — `expenses` and `loans` can't be (§6c: FalconGo/GORM
   recreates them and four live FalconGo routes depend on `expenses`).

**C. The four extra columns + `description`.** Approve adding `payment_method`, `company`,
`car_no_plate`, `paid_by` (+ `import_source_id`) to `banksms.transactions` for feature parity. And:
does `description` become its own column, or a `banksms.notes` row?

**D. Overrides table — spec §4's mandatory stop, "the hardest thing to change later":**
- **(a)** JSONB `overrides` column — simplest, no audit trail
- **(b)** `transaction_overrides` table `(transaction_id, field, value, actor, set_at)` — full history
- **(c)** append-only event log, state as a fold — max auditability, heaviest

The spec forbids me choosing. I'll note only that §8's `GET /transactions/:id/history` implies ≥ (b).

**E. Related to D — where do non-parser amounts live?** Spec §4 says `parsed_*` are parser-owned and
manual rows must reject writes to them. Imported and manually-created rows have real amounts that no
parser produced.
- **(e1)** write `parsed_*` for `source='import'|'manual'` with `parse_method='manual'`,
  `parser_version=0` — one read path, muddier ownership
- **(e2)** add user-owned `amount`/`occurred_at`/`direction`; read `COALESCE(override, user, parsed_*)`
  — clean ownership, touches every read query

**F. Smaller confirmations:**
- Acceptance target → **34/34 via tier 1** + 444 chatter → `ignored` (replacing "23/23")?
- Seed patterns stored in **post-normalization** form (§3)?
- New routes under **`/api/v1`** (matching existing), with `/healthz`/`/readyz` at root?
- Should I **clone and read `apex-react`** before finalizing DDL?
- Where do the **dumps** live permanently? (currently a session temp dir + `/tmp` on the VPS)
- 🔴 Fix the **`JWT_SECRET` → `"secret"` fallback** (§1) as part of Phase 3 hardening?

**Not proceeding to Phase 1 until you confirm.** The triage thresholds (spec §7.3) remain a separate
later stop — I now have the 479-message corpus needed to give you real confusion counts when we
reach it.
