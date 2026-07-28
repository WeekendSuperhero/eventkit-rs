# EventKit authorization — two coupled fixes

**Status: IMPLEMENTED 2026-07-26.** Found during the 2026-07-25 macOS entitlements sweep in
the consuming app. `eventkit-rs` is excluded from that workspace (root `Cargo.toml`) and
published separately, so this is its own change + release (was v0.5.6 when planned).

## What shipped, and where it deviated

All three steps landed. Two deviations from the plan as written, both forced by facts the
plan didn't have:

1. **The seam is a pure function, not a trait.** The plan proposed
   `trait AuthorizationSource { fn status(&self, entity) -> AuthorizationStatus }`. Shipped
   instead: `authorization_verdict(need: AccessNeed, status: AuthorizationStatus) -> AuthVerdict`,
   which takes the status as a PARAMETER. Same goal — tests drive all five statuses with no
   EventKit and no dependence on the host's TCC state — but strictly simpler: a trait would
   still have needed a manager (and therefore a real `EKEventStore`) to exercise the
   `ensure_*` path, whereas the pure function needs nothing. `AuthRefusal` carries WHY a
   check refused so the three refusals stay distinct errors.

2. **The store cache is THREAD-LOCAL, not a `OnceLock` singleton, and lives inside `new()`.**
   The plan flagged the `!Send + !Sync` constraint but not its consequence:
   `mcp.rs::EventKitServer` documents a load-bearing invariant — handlers keep EventKit
   values stack-local and never hold one across an `.await`, which is what makes the
   generated futures `Send` and lets the server run on a normal multi-thread tokio runtime
   *without rmcp's `local` feature*. Caching a store in the server struct would have broken
   that and rippled into the consuming app. A `OnceLock` global is impossible for the same
   `!Send + !Sync` reason. Thread-local caching preserves the invariant (one store per worker
   thread instead of one per call), and putting the lookup inside `RemindersManager::new()` /
   `EventsManager::new()` meant **zero call-site changes** at the ~68 construction sites —
   `Retained` clone is a cheap retain.

### Incident: the optional item caused a live outage (2026-07-26)

The plan's optional third item — "also honour `EKEventStoreChanged`" — was
implemented as a cheaper store-identity check, because the notification is
posted on the main actor and a headless MCP server has no runloop. That check
called `EKEventStore.eventStoreIdentifier()` inside `StoreCache::build`, which
runs on every `Manager::new()`.

`objc2` declares that accessor as returning a non-null `Retained<NSString>`.
EventKit returns **NULL** to a process that has not been authorized yet, so
objc2 aborted the thread:

```
PANIC on thread 'tokio-rt-worker' at objc2-event-kit-0.3.2/.../EKEventStore.rs:71:5:
unexpected NULL returned from -[EKEventStore eventStoreIdentifier]
```

The panic landed BEFORE `ensure_full_access`, so `request_access()` never ran
and **the TCC consent dialog never appeared**. From the outside the reminders
tools simply did nothing, with no permission prompt and no error — the exact
failure mode this whole plan exists to prevent, reintroduced by its own optional
extra.

It was invisible to every test because the dev host had already granted access,
so the accessor returned non-null there. It only reproduced in the app bundle,
which has a separate (ungranted) TCC identity.

**Resolution: the identity check was removed, not patched.** It guarded a
hypothesis (a recreated calendar database) that was never demonstrated to
matter, and the live test in `tests/live_eventkit_store_cache.rs` had already
shown a cached store observes external writes fine. `StoreCache::build` is now
inert — it constructs the store and nothing else — with the rule documented on
it and a regression test (`constructing_a_manager_touches_no_eventkit_accessor`).

**The general rule this establishes:** never call an EventKit accessor on a path
that runs before authorization. `objc2`'s non-null bindings turn "unauthorized"
into a thread abort, and an abort before `ensure_*` silently suppresses the
consent prompt.

Two things the plan didn't anticipate:

- **The same write-only bug existed a second time, at the MCP layer.** `mcp.rs::auth_remediation`
  counted `WriteOnly` as granted, so `auth_status` told a write-only user everything was fine
  while every read came back silently empty. Fixed with the same rule (only `FullAccess` is
  granted) and its own regression test.
- **`app.rs`'s "20 constructions" overstates its own cost** — it is CLI dispatch, so only one
  runs per invocation. The real hot path was `mcp.rs`, which built a manager per tool call
  (48 sites). Both benefit from the cache regardless.

Also fixed while here: `ci-check.sh` had been failing on pre-existing lints unrelated to
this plan. The blocking-bridge lock pairs (`Mutex` + `Condvar`, four of them, used to wait
on Obj-C completion blocks) moved from `std::sync` to `parking_lot`, which is what the
repo's `clippy.toml` actually prescribes — it names `std::sync::Condvar` interop as the one
legitimate std-Mutex use and then says to *"pair parking_lot::Condvar with parking_lot::Mutex
instead"*. Beyond the lint this is a real fix: `std::sync::Mutex` POISONS, so a panic inside
a completion block while holding the lock would make every subsequent `lock()` panic and
permanently brick the manager, rather than failing one call. parking_lot doesn't poison,
which also removed eight `.unwrap()`s. It was already in the tree via tokio, so the direct
dependency adds no compile unit. One unrelated `manual_filter` fixed too. `ci-check.sh` is
now green with no lint suppressions.

---

## Original plan follows

Two defects, and they **must land together** — fixing the second without the first
introduces a regression. Neither is an entitlement problem: the entitlement
(`personal-information.calendars`) and usage strings are correct. This is the *runtime API*
layer.

---

## Defect 1 — `WriteOnly` is treated as authorized, so reads return empty instead of failing

`imp.rs:431` (reminders) and `imp.rs:2267` (events):

```rust
AuthorizationStatus::WriteOnly => Ok(()), // Can still read with write-only in some cases
```

That comment is contradicted by Apple:

> "Your app **can't request read-only access** to either events or reminders. To read events
> or reminders from the event store, your app needs **full access**."
> — *Accessing the event store*

> "Your app can create events, but it can't access any of the existing calendars and events
> on the device, **including events your app created**. API calls to read event data from
> the event store **don't return any events**."
> — *requestWriteOnlyAccessToEvents*

**Why this matters more than a wrong error code.** Reads don't fail — they *succeed and
return nothing*. `ensure_authorized()` returns `Ok`, the fetch runs, and the caller gets an
empty list. For an agent product that means the model confidently tells the user *"you have
nothing on your calendar"* when it simply lacks permission. A wrong answer is worse than an
error, and it is indistinguishable from a genuinely empty calendar.

Also note **Reminders has no write-only tier at all** — only
`requestFullAccessToReminders` exists — so `imp.rs:431` is unreachable-but-wrong, while
`imp.rs:2267` (events) is live.

### The fix

`ensure_authorized()` is the wrong shape: it answers one question ("may I proceed?") for two
different operations. Split the intent:

```rust
/// Full access — required for ANY read/list/update path. `WriteOnly` is an error here:
/// Apple returns no events at all to a write-only client.
fn ensure_full_access(&self) -> Result<()>;

/// Create-only paths. `WriteOnly` and `FullAccess` both satisfy this.
fn ensure_write_access(&self) -> Result<()>;
```

- `ensure_full_access`: `FullAccess => Ok`; `WriteOnly => Err(AuthorizationWriteOnly)`;
  `NotDetermined =>` request, then re-check; `Denied`/`Restricted` unchanged.
- `ensure_write_access`: `FullAccess | WriteOnly => Ok`; rest as above.
- Add a distinct error variant (`AuthorizationWriteOnly`) rather than reusing
  `AuthorizationDenied` — the remedy the user needs is different ("grant full access in
  System Settings", not "you denied us"), and the app surfaces these strings.
- Point every list/get/search/update/delete method at `ensure_full_access`. Only genuine
  create-only paths, if any are exposed separately, get `ensure_write_access`.
- `mcp.rs:107` already reports the raw status string to callers — keep it, and make sure the
  MCP tool error text names full access explicitly.

### Tests

- `WriteOnly` ⇒ every read/list path returns `Err(AuthorizationWriteOnly)`, never `Ok(vec![])`.
  This is the regression that matters: assert on the *error*, because `Ok(empty)` is exactly
  the bug.
- `WriteOnly` ⇒ a create path still succeeds.
- `FullAccess` ⇒ both succeed. `Denied`/`Restricted` ⇒ unchanged errors.
- Reminders + events arms covered separately (the reminders `WriteOnly` arm is unreachable in
  practice; keep it as a defensive `Err`, not `Ok`).

Status is a process-global TCC fact, so these need the status injectable — see "Testability"
below.

---

## Defect 2 — a fresh `EKEventStore` per call, and `reset()` is never called

`app.rs` constructs a manager **20 times** (`RemindersManager::new()` / `EventsManager::new()`),
each doing `EKEventStore::new()` (`imp.rs:364`, `imp.rs:2202`). Apple:

> "Set up your app to instantiate and use a **single instance** of `EKEventStore` that manages
> all reminder-related tasks. An `EKEventStore` object requires a **significant amount of time
> to initialize and release**." — *Managing location-based reminders*

> "Releasing an event store instance before other EventKit objects may result in an error."
> — *Accessing the event store*

So every calendar tool call pays full EventKit init/teardown.

### ⚠️ The coupling — read before "just" caching the store

Apple also requires:

> "If you request events before prompting people for access with this method, you'll need to
> **reset the event store with the `reset()` method**" to see data after the grant.

Nothing in `eventkit-rs` calls `reset()`. `refreshSourcesIfNecessary()`
(`imp.rs:1222,1231,2708,2726`) is a **different** API — it refreshes account sources, not
post-authorization state.

Today that's harmless *precisely because* the per-call construction is wasteful: every call
gets a fresh store that reads current authorization state. **The performance bug is masking
the correctness bug.**

Hoisting to a shared singleton — the obvious optimization, and the one Apple's guidance points
at — **introduces** the `reset()` bug: a user who grants access in System Settings mid-session
keeps getting empty results until they restart the app. That is the same
silently-empty-results failure as Defect 1, arriving by a different route.

### The fix, in one change

1. **One store per manager, manager cached** — `OnceLock`/`OnceCell` holding the store, or a
   long-lived manager the caller owns. `EKEventStore` is `!Send + !Sync` and
   `CLLocationManager` already has a documented main-thread constraint
   (`location.rs:29`), so the cache must respect thread affinity — don't paper over it with a
   `Mutex` that permits cross-thread use.
2. **Track the last-observed `EKAuthorizationStatus` alongside the store.** On every
   `ensure_*`, compare current status to the stored one; on any transition (notably
   `NotDetermined`/`WriteOnly` → `FullAccess`), call `store.reset()` before proceeding.
3. **Also honour `EKEventStoreChanged`** if cheap — the notification EventKit posts when the
   store changes underneath you. Optional; the status-transition check is the load-bearing part.

### Tests

- Simulated transition `WriteOnly → FullAccess` on a cached store calls `reset()` exactly
  once and subsequent reads are not empty. This is the regression guard for the coupling; it
  is the test that would fail if someone caches the store without adding `reset()`.
- Repeated calls reuse one store (assert construction count, not timing).
- No `reset()` when status is unchanged (it discards uncommitted changes — don't call it
  gratuitously).

---

## Testability — the real blocker for both

`EKEventStore.authorizationStatus(for:)` is a **class method reading process-global TCC
state**, so today the authorization logic can't be unit-tested at all: a test run on a machine
with Calendar granted takes a different branch than CI.

Extract the status read behind a seam before either fix:

```rust
trait AuthorizationSource { fn status(&self, entity: EKEntityType) -> AuthorizationStatus; }
```

Real impl calls EventKit; tests inject each of the five statuses. Everything above depends on
this — **do it first**, otherwise both fixes ship unverified and the whole point was that these
bugs are invisible without a test.

## Sequencing

1. `AuthorizationSource` seam + tests for the five statuses (no behaviour change).
2. Defect 1: split `ensure_full_access` / `ensure_write_access`, new error variant, repoint
   callers. Behaviour change — a `WriteOnly` user now sees an error instead of silence.
3. Defect 2: cache the store **and** add `reset()`-on-transition in the same commit.
4. Bump + release `eventkit-rs`; update the consumer's pin.

## Consumer-side notes

- `app-api/src/bridge.rs:137` runs `eventkit::mcp::serve_on(transport)` **in-process**, so
  TCC attributes to the host app bundle — no entitlement change needed for any of this.
- After step 2, the app surfaces a new error for write-only users. Make sure the MCP tool
  error text tells them to grant *full* access, since "denied" would be misleading.
- The consuming app's `src-tauri/TCC_PERMISSIONS.md` tracks both items in its audit table;
  mark them resolved there when this ships.
