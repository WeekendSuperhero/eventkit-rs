---
created: 2026-05-29T04:27
updated: 2026-08-04T00:00
---
# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.6.0] - 2026-08-04

Versions 0.3.0 through 0.5.x shipped without changelog entries; their changes are
folded in here rather than reconstructed after the fact.

### Fixed
- **EventKit store exhaustion (critical)** — `RemindersManager::new()` and `EventsManager::new()` each constructed a fresh `EKEventStore` on *every* call, and the MCP layer builds a manager per tool call. A busy session allocated one store per call until the calendar daemon refused the process outright:
  `EKCADErrorDomain 1021 — "This process has too many EKEventStore instances. Use fewer event stores."`
  Stores are now cached **per thread** (`StoreCache`), so the process holds two rather than one per call. Apple: *"instantiate and use a single instance of `EKEventStore` … an `EKEventStore` object requires a significant amount of time to initialize and release."* A test asserts there is exactly one construction site so this cannot regress silently.
- **Reads silently returned empty under write-only access** — `WriteOnly` was treated as authorized, so `ensure_authorized()` returned `Ok` and every read succeeded with *nothing in it*. Apple returns no events at all to a write-only client, so an agent would confidently report an empty calendar to a user who simply hadn't granted full access. Authorization is now split into `ensure_full_access` / `ensure_write_access`, with a distinct `AuthorizationWriteOnly` error naming the right remedy.
- **Store went stale after a mid-session grant** — caching the store required the other half of Apple's contract: the store is now `reset()` on an authorization *transition*, so granting access in System Settings takes effect without an app restart. Not called when the status is unchanged, since `reset()` discards uncommitted changes.
- **Consent dialog could be suppressed entirely** — an `eventStoreIdentifier()` call on the construction path aborted the thread on any machine that had not yet granted access (`objc2` declares the accessor non-null; EventKit returns NULL when unauthorized). The abort landed before `ensure_full_access`, so access was never requested and no TCC prompt ever appeared. Manager construction is now inert — it must never call an EventKit accessor.
- **`auth_status` misreported write-only as granted** — the same write-only bug at the MCP layer; only `FullAccess` counts as granted.
- **A panicking tool permanently disabled background tasks** — a task is marked terminal only *after* `call_tool` returns, so an unwind stranded it in `Working`: TTL pruning skipped it and it held an active slot forever. After 16 such panics, *every* subsequent task was refused for the life of the process, and `tasks/result` waiters blocked on a slot nothing would fill. A drop guard now marks the task `Failed` and publishes a result during the unwind.
- **A cancelled task could be downgraded** — a worker finishing just after `tasks/cancel` overwrote `Cancelled` with `Completed`/`Failed` and pushed a second status notification. Terminal is now final.
- **Poisoned locks could brick a manager** — the blocking bridges that wait on Obj-C completion blocks moved from `std::sync::Mutex` to `parking_lot`, which does not poison. A panic inside a completion block previously made every later `lock()` panic, turning one failed call into a permanently dead manager. Also removed eight `.unwrap()`s.
- **Schema generation** — resolved stderr noise by explicitly mapping `usize` / `u8` to standard schemars types.
- **CI/CD workflows** — fixed GitHub Actions workflow and release job yaml configurations.

### Added
- **MCP task surface (SEP-1686)** — slow tools (full-calendar scans, batch operations) can be invoked task-augmented, returning a task id immediately. Bounded memory (16 concurrent, 256 tracked, 1h TTL), real cancellation, and a pushed `notifications/tasks/status` on every transition so clients need not poll.
- **MCP Server** — `auth_status` and `request_access` tools, tools to set due timezone, geofence, and event availability, and an embedded `Info.plist` to trigger macOS TCC privacy prompts.
- **EventKit Core** — exposed `URL`, `availability`, `structured_location`, `due_date_timezone`, and `attachments_count`, plus raw reflection via `dump_reminder_raw` / `dump_reminder_private`.
- **Testing** — live EventKit tests (including store-cache checks against external writes) and MCP smoke tests.
- **PR description workflow** — GitHub Actions workflow to update PR descriptions on open and edit.

### Changed
- **Dependencies** — `rmcp` to 2.2 (from the 1.3/1.4 line), `tokio` to 1.51.
- **EventKit Core** — creation and updates refactored onto `Draft` and `Patch` structs.
- **Error messages** — authorization errors now carry TCC remediation steps.
- **Cross-platform compilation** — `objc2`/EventKit gated behind `cfg(target_os = "macos")`, with platform-split modules so the crate compiles as an empty shell elsewhere.
- **CI & Infrastructure** — testing migrated from `cargo test` to `cargo nextest`; universal build workflow strategy updated.
- **Tracing logs** — lighter logging.
- **Documentation** — refreshed `README.md`; added `AUTHORIZATION_PLAN.md` recording the authorization/caching work and its incident history.
- **Project cleanup** — reorganized `Cargo.toml` metadata and features, replaced explicit closures with function pointers, clippy annotations.

## [0.2.0] - 2025-02-10

### Added

- **MCP Server**: Built-in Model Context Protocol (MCP) server via `--mcp` flag
  - Exposes all Calendar and Reminders functionality as MCP tools
  - Runs over stdio transport for easy integration with AI assistants
  - Gated behind the `mcp` feature (enabled by default)
- `mcp` module with `EventKitServer` and `run_mcp_server()` public API

### Changed

- `mcp` feature is now included in default features (`events`, `reminders`, `mcp`)
- CLI `command` field is now optional to support the top-level `--mcp` flag

### Fixed

- Event save/remove operations now use the explicit `commit: true` variants
  - `saveEvent:span:error:` replaced with `saveEvent:span:commit:error:` (commit = true)
  - `removeEvent:span:error:` replaced with `removeEvent:span:commit:error:` (commit = true)
  - Ensures events are committed to the Calendar database immediately, consistent with how reminders and calendars were already handled

## [0.1.0] - 2024-XX-XX

### Added

- Initial release
- `RemindersManager` for full CRUD operations on macOS Reminders
  - Create, read, update, delete reminders
  - List reminder calendars (lists)
  - Mark reminders complete/incomplete
  - Filter by calendar and completion status
- `EventsManager` for calendar event management
  - Create, read, update, delete calendar events
  - Fetch events by date range
  - Support for all-day events
  - List calendars
- Authorization handling for both reminders and calendar access
- CLI tool (`eventkit`) with subcommands:
  - `eventkit reminders` - Manage reminders
  - `eventkit events` - Manage calendar events
  - `eventkit status` - Check authorization status
- Comprehensive documentation
- GitHub Actions for CI/CD
- MIT License

### Known Limitations

- macOS only (10.14+)
- Recurring events show as individual occurrences
- No support for event invitations/attendees management

[Unreleased]: https://github.com/weekendsuperhero/eventkit-rs/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/weekendsuperhero/eventkit-rs/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/weekendsuperhero/eventkit-rs/releases/tag/v0.1.0
