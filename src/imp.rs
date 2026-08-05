use block2::RcBlock;
use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Timelike};
use objc2::Message;
use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2_event_kit::{
    EKAlarm, EKAlarmProximity, EKAlarmType, EKAuthorizationStatus, EKCalendar,
    EKCalendarEventAvailabilityMask, EKCalendarItem, EKEntityType, EKEvent, EKEventAvailability,
    EKEventStatus, EKEventStore, EKRecurrenceDayOfWeek, EKRecurrenceEnd, EKRecurrenceFrequency,
    EKRecurrenceRule, EKReminder, EKSource, EKSpan, EKStructuredLocation, EKWeekday,
};
use objc2_foundation::{
    NSArray, NSCalendar, NSDate, NSDateComponents, NSError, NSNumber, NSString,
};
// parking_lot for the lock pair, std only for `Arc`.
//
// These mutexes are paired with a `Condvar` to block on an Obj-C completion
// block. `std::sync::Mutex` POISONS: a panic inside the completion block while
// holding the lock would make every later `lock()` panic, permanently bricking
// the manager instead of failing one call. parking_lot doesn't poison, which
// also removes the `.unwrap()` from every acquisition.
use parking_lot::{Condvar, Mutex};
use std::sync::Arc;
use thiserror::Error;

#[cfg(feature = "location")]
#[path = "location.rs"]
pub mod location;

#[cfg(feature = "mcp")]
#[path = "mcp.rs"]
pub mod mcp;

/// Errors that can occur when working with EventKit
#[derive(Error, Debug)]
pub enum EventKitError {
    #[error("Authorization denied")]
    AuthorizationDenied,

    #[error("Authorization restricted by system policy")]
    AuthorizationRestricted,

    /// Write-only access was granted, but the operation needs to READ.
    ///
    /// Distinct from [`EventKitError::AuthorizationDenied`] because the remedy
    /// differs — the user must grant FULL access in System Settings, not
    /// reverse a denial. Apple returns no events at all to a write-only
    /// client, so treating this as authorized yields a silently empty result
    /// rather than an error.
    #[error(
        "Write-only access granted — full access is required to read calendars, events, or \
         reminders. Grant full access in System Settings › Privacy & Security."
    )]
    AuthorizationWriteOnly,

    #[error("Authorization not determined")]
    AuthorizationNotDetermined,

    #[error("Failed to request authorization: {0}")]
    AuthorizationRequestFailed(String),

    #[error("No default calendar")]
    NoDefaultCalendar,

    #[error("Calendar not found: {0}")]
    CalendarNotFound(String),

    #[error("Item not found: {0}")]
    ItemNotFound(String),

    #[error("Failed to save: {0}")]
    SaveFailed(String),

    #[error("Failed to delete: {0}")]
    DeleteFailed(String),

    #[error("Failed to fetch: {0}")]
    FetchFailed(String),

    #[error("EventKit error: {0}")]
    EventKitError(String),

    #[error("Invalid date range")]
    InvalidDateRange,

    /// Returned when a string passed as a URL fails strict RFC 3986
    /// validation (`+[NSURL URLWithString:encodingInvalidCharacters:NO]`
    /// returned nil — invalid scheme, illegal characters, etc.).
    #[error("Invalid URL: {0}")]
    InvalidURL(String),
}

/// Backward compatibility alias
pub type RemindersError = EventKitError;

/// Result type for EventKit operations
pub type Result<T> = std::result::Result<T, EventKitError>;

/// Represents a reminder item with its properties
#[derive(Debug, Clone)]
#[allow(non_snake_case)]
pub struct ReminderItem {
    /// Unique identifier for the reminder
    pub identifier: String,
    /// Title of the reminder
    pub title: String,
    /// Optional notes/description
    pub notes: Option<String>,
    /// Whether the reminder is completed
    pub completed: bool,
    /// Priority (0 = none, 1-4 = high, 5 = medium, 6-9 = low)
    pub priority: usize,
    /// Calendar/list the reminder belongs to
    pub calendar_title: Option<String>,
    /// Calendar/list identifier
    pub calendar_id: Option<String>,
    /// Due date for the reminder
    pub due_date: Option<DateTime<Local>>,
    /// Start date (when to start working on it)
    pub start_date: Option<DateTime<Local>>,
    /// Completion date (when it was completed)
    pub completion_date: Option<DateTime<Local>>,
    /// External identifier for the reminder (server-provided)
    pub external_identifier: Option<String>,
    /// Location associated with the reminder
    pub location: Option<String>,
    /// URL associated with the reminder
    #[allow(non_snake_case)]
    pub URL: Option<String>,
    /// Creation date of the reminder
    pub creation_date: Option<DateTime<Local>>,
    /// Last modified date of the reminder
    pub last_modified_date: Option<DateTime<Local>>,
    /// Timezone of the reminder
    pub timezone: Option<String>,
    /// Timezone applied specifically to the due date — distinct from
    /// `timezone`, which is the item-level zone. EventKit lets these differ
    /// so a reminder can be "due at 9am New York time" regardless of the
    /// item's own zone or the device zone.
    pub due_date_timezone: Option<String>,
    /// Geofence attached to the reminder via a location-based alarm.
    /// `Some` when at least one alarm has a `structuredLocation` and
    /// non-`None` proximity. EventKit has no `structuredLocation` property
    /// directly on `EKReminder` / `EKCalendarItem` — it lives on the alarm.
    pub structured_location: Option<StructuredLocation>,
    /// Identifier of the parent reminder (when this reminder is a subtask).
    /// Read via KVC because the underlying type is the private `EKObjectID`.
    pub parent_id: Option<String>,
    /// Number of file attachments. Full metadata is not yet surfaced.
    pub attachments_count: usize,
    /// Whether the reminder has alarms
    pub has_alarms: bool,
    /// Whether the reminder has recurrence rules
    pub has_recurrence_rules: bool,
    /// Whether the reminder has attendees
    pub has_attendees: bool,
    /// Whether the reminder has notes
    pub has_notes: bool,
    /// Attendees on this reminder (usually empty, possible on shared lists)
    pub attendees: Vec<ParticipantInfo>,
}

/// Input for `RemindersManager::create_reminder`. All fields except `title`
/// are optional. Use `..Default::default()` for the unset ones.
#[derive(Debug, Clone, Default)]
#[allow(non_snake_case)]
pub struct ReminderDraft<'a> {
    pub title: &'a str,
    pub notes: Option<&'a str>,
    pub calendar_title: Option<&'a str>,
    pub priority: Option<usize>,
    pub due_date: Option<DateTime<Local>>,
    pub start_date: Option<DateTime<Local>>,
    #[allow(non_snake_case)]
    pub URL: Option<&'a str>,
    pub location: Option<&'a str>,
    /// Rich location for Reminders.app's location chip — writes
    /// `EKReminder.structuredLocation`. This is the field iCloud Reminders
    /// actually persists; the plain-string `location` above is a legacy
    /// CalDAV field that iCloud silently drops.
    pub structured_location: Option<&'a StructuredLocation>,
    /// IANA zone identifier for the due date, e.g. `"America/Los_Angeles"`.
    pub due_date_timezone: Option<&'a str>,
}

/// Input for `RemindersManager::update_reminder`. Each field uses one of:
/// `None` (don't touch), `Some(value)` (set), and for `Option<Option<T>>`
/// fields, `Some(None)` (clear). Build with `..Default::default()`.
#[derive(Debug, Clone, Default)]
#[allow(non_snake_case)]
pub struct ReminderPatch<'a> {
    pub title: Option<&'a str>,
    pub notes: Option<&'a str>,
    pub completed: Option<bool>,
    pub priority: Option<usize>,
    pub due_date: Option<Option<DateTime<Local>>>,
    pub start_date: Option<Option<DateTime<Local>>>,
    pub calendar_title: Option<&'a str>,
    #[allow(non_snake_case)]
    pub URL: Option<Option<&'a str>>,
    pub location: Option<Option<&'a str>>,
    /// Rich location (the iCloud-persisted chip). `Some(Some(loc))` sets,
    /// `Some(None)` clears, `None` leaves untouched.
    pub structured_location: Option<Option<&'a StructuredLocation>>,
    /// Same `Option<Option<...>>` semantics: `Some(Some("America/LA"))` sets,
    /// `Some(None)` clears, `None` leaves untouched.
    pub due_date_timezone: Option<Option<&'a str>>,
    /// Explicit completion date. Apple's `setCompletionDate:` is the
    /// authoritative completion toggle — a non-nil value implies
    /// `isCompleted = YES` and a nil value implies `NO`. We apply this
    /// **after** `completed` in `update_reminder`, so when both are
    /// provided the date wins.
    pub completion_date: Option<Option<DateTime<Local>>>,
}

/// Type of calendar/source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CalendarType {
    Local,
    CalDAV,
    Exchange,
    Subscription,
    Birthday,
    Unknown,
}

/// An account source (iCloud, Local, Exchange, etc.)
#[derive(Debug, Clone)]
pub struct SourceInfo {
    pub identifier: String,
    pub title: String,
    pub source_type: String,
}

/// Represents a calendar (reminder list or event calendar).
#[derive(Debug, Clone)]
pub struct CalendarInfo {
    /// Unique identifier
    pub identifier: String,
    /// Title of the calendar
    pub title: String,
    /// Source name (e.g., iCloud, Local)
    pub source: Option<String>,
    /// Source identifier
    pub source_id: Option<String>,
    /// Calendar type
    pub calendar_type: CalendarType,
    /// Whether items can be added/modified/deleted
    pub allows_modifications: bool,
    /// Whether the calendar itself can be modified (renamed/deleted)
    pub is_immutable: bool,
    /// Whether this is a URL-subscribed read-only calendar
    pub is_subscribed: bool,
    /// Calendar color as RGBA (0.0-1.0)
    pub color: Option<(f64, f64, f64, f64)>,
    /// Entity types this calendar supports ("event", "reminder")
    pub allowed_entity_types: Vec<String>,
    /// Which `EventAvailability` values this calendar accepts on its events
    /// — string forms of `EKCalendarEventAvailabilityMask`: e.g.
    /// `["busy", "free", "tentative"]`. Empty when none are supported
    /// (`EKCalendarEventAvailabilityNone`, common on reminder-only calendars).
    /// Useful pre-flight check before `EventsManager::set_event_availability`.
    pub supported_event_availabilities: Vec<String>,
}

/// Proximity trigger for a location-based alarm.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlarmProximity {
    /// No proximity trigger.
    #[default]
    None,
    /// Trigger when entering the location.
    Enter,
    /// Trigger when leaving the location.
    Leave,
}

/// A structured location for geofenced alarms.
#[derive(Debug, Clone)]
pub struct StructuredLocation {
    /// Display title for the location.
    pub title: String,
    /// Latitude of the location.
    pub latitude: f64,
    /// Longitude of the location.
    pub longitude: f64,
    /// Geofence radius in meters.
    pub radius: f64,
}

/// An alarm attached to a reminder or event.
#[derive(Debug, Clone, Default)]
pub struct AlarmInfo {
    /// Offset in seconds before the due date (negative = before).
    pub relative_offset: Option<f64>,
    /// Absolute date for the alarm (ISO 8601 string).
    pub absolute_date: Option<DateTime<Local>>,
    /// Proximity trigger (enter/leave geofence).
    pub proximity: AlarmProximity,
    /// Location for geofenced alarms.
    pub location: Option<StructuredLocation>,
    /// Email address for email-type alarms (CalDAV server-side notification).
    pub email_address: Option<String>,
    /// Custom alarm sound name (audio-type alarms).
    pub sound_name: Option<String>,
    /// URL opened when the alarm fires (procedure-type alarm). Distinct
    /// from `EKCalendarItem.URL` — that's lowercase `url` on `EKAlarm`.
    /// Apple deprecated this property in macOS 10.9 but the API still
    /// functions; we surface it for parity with the framework.
    pub url: Option<String>,
    /// Derived alarm type — Display/Audio/Procedure/Email. Apple infers
    /// this from which of the optional fields above are set. Read-only on
    /// output; ignored on input.
    pub alarm_type: AlarmType,
}

/// EKAlarm.type — derived by Apple from which optional fields are set:
/// `soundName` → Audio, `url` → Procedure, `emailAddress` → Email, else Display.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AlarmType {
    #[default]
    Display,
    Audio,
    Procedure,
    Email,
    Unknown,
}

/// How often a recurrence repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecurrenceFrequency {
    #[default]
    Daily,
    Weekly,
    Monthly,
    Yearly,
}

/// When a recurrence ends.
#[derive(Debug, Clone, Default)]
pub enum RecurrenceEndCondition {
    /// Repeats forever.
    #[default]
    Never,
    /// Ends after a number of occurrences.
    AfterCount(usize),
    /// Ends on a specific date.
    OnDate(DateTime<Local>),
}

/// A recurrence rule describing how a reminder or event repeats.
#[derive(Debug, Clone, Default)]
pub struct RecurrenceRule {
    /// How often it repeats (daily, weekly, monthly, yearly).
    pub frequency: RecurrenceFrequency,
    /// Repeat every N intervals (e.g., every 2 weeks).
    pub interval: usize,
    /// When the recurrence ends.
    pub end: RecurrenceEndCondition,
    /// Days of the week (1=Sun..7=Sat) for weekly/monthly rules.
    pub days_of_week: Option<Vec<u8>>,
    /// Days of the month (1-31, negatives count from end) for monthly rules.
    pub days_of_month: Option<Vec<i32>>,
    /// Months of the year (1=Jan..12=Dec) for yearly rules.
    pub months_of_year: Option<Vec<i32>>,
    /// Weeks of the year (1..=53, negatives count from end). Yearly rules only.
    pub weeks_of_year: Option<Vec<i32>>,
    /// Days of the year (1..=366, negatives count from end). Yearly rules only.
    pub days_of_year: Option<Vec<i32>>,
    /// Set positions — filter applied after the other fields. E.g. with
    /// `frequency = Monthly`, `days_of_week = [2]` (Monday), and
    /// `set_positions = [1]` you get "the first Monday of every month".
    /// Negative values count from the end.
    pub set_positions: Option<Vec<i32>>,
}

/// How long a blocking bridge waits for an EventKit completion block.
///
/// Every condvar wait in this file parks a thread until an Obj-C completion
/// block fires. Unbounded, that is a permanent thread wedge whenever the block
/// never arrives — and the MCP server runs these on tokio worker threads it
/// cannot spare, so a wedged wait removes a worker from the pool for the life
/// of the process. Bounding it turns "the server silently stopped answering"
/// into an error the caller can see and retry.
const COMPLETION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Authorization gets a longer budget than data calls: it waits on a HUMAN
/// dismissing the TCC consent dialog, not on the framework. Still bounded — a
/// process that can never show a dialog (headless, no TCC grant, no GUI
/// session) would otherwise leave `requestFullAccessTo*` outstanding forever.
const AUTHORIZATION_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// Park until a completion block fills the slot, or `timeout` elapses.
///
/// Returns `false` on timeout, leaving the slot untouched so the caller decides
/// which error to report. The loop is required because a condvar may wake
/// spuriously.
fn wait_for_completion<T>(
    cvar: &Condvar,
    guard: &mut parking_lot::MutexGuard<'_, Option<T>>,
    timeout: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while guard.is_none() {
        if cvar.wait_until(guard, deadline).timed_out() {
            return false;
        }
    }
    true
}

/// A thread's cached `EKEventStore` plus the authorization status last
/// observed through it.
///
/// Apple: *"Set up your app to instantiate and use a **single instance** of
/// `EKEventStore` that manages all reminder-related tasks. An `EKEventStore`
/// object requires a **significant amount of time to initialize and
/// release**."* Constructing one per call made every tool invocation pay full
/// EventKit init/teardown.
///
/// **Why thread-local and not a `OnceLock` global:** `EKEventStore` is
/// `!Send + !Sync`. A global would demand both. It also has to stay that way —
/// `mcp.rs`'s handlers deliberately keep these values stack-local and never
/// hold one across an `.await`, which is what makes the generated futures
/// `Send` and lets the server run on a normal multi-thread tokio runtime
/// without rmcp's `local` feature. A thread-local keeps that invariant intact
/// while still amortising construction: one store per worker thread instead of
/// one per call.
struct StoreCache {
    store: Retained<EKEventStore>,
    /// Status observed the last time this store was used. `None` until the
    /// first check. Drives the `reset()` decision below.
    last_status: Option<AuthorizationStatus>,
}

impl StoreCache {
    /// Build a cache entry around a fresh store.
    ///
    /// **Do not call any EventKit accessor here.** Construction happens on
    /// EVERY `Manager::new()`, including long before authorization exists, and
    /// most `EKEventStore` accessors are declared non-null by `objc2` while
    /// actually returning NULL to an unauthorized process — which aborts the
    /// calling thread.
    ///
    /// This bit us: a `eventStoreIdentifier()` call added here to detect a
    /// recreated calendar database panicked with *"unexpected NULL returned
    /// from -[EKEventStore eventStoreIdentifier]"* on any machine that hadn't
    /// granted access yet. The panic landed BEFORE `ensure_full_access`, so
    /// `request_access()` never ran and the TCC consent dialog never appeared
    /// — the feature looked simply broken, with no permission prompt. It was
    /// invisible in tests because the dev host had already granted access.
    fn build() -> Self {
        Self {
            store: unsafe { EKEventStore::new() },
            last_status: None,
        }
    }

    /// Reconcile the cached store with the CURRENT authorization status,
    /// resetting it when the grant changed underneath us.
    ///
    /// **This is the half that must never be separated from the caching.**
    /// Apple: *"If you request events before prompting people for access with
    /// this method, you'll need to reset the event store with the `reset()`
    /// method"* to see data after the grant. Per-call construction used to make
    /// that free — every call got a store that already reflected current state
    /// — so caching without this would reintroduce the same silently-empty
    /// results by a different route: a user who grants access in System
    /// Settings mid-session would keep getting nothing until an app restart.
    ///
    /// `refreshSourcesIfNecessary()` is NOT a substitute; it refreshes account
    /// sources, not post-authorization state.
    ///
    /// **Why calling `reset()` here is safe.** Apple's full contract is
    /// harsher than the reset-after-grant note suggests: *"All existing
    /// objects created or retrieved using this store are disassociated from it
    /// and are **invalid**."* Two properties keep that from biting:
    ///
    /// 1. `reconcile` runs at the TOP of `ensure_access`, before the calling
    ///    operation has fetched anything, so no live `EKObject` from this
    ///    store is in flight when the reset lands.
    /// 2. Nothing this crate returns to a caller is a live EventKit object.
    ///    `ReminderItem` / `EventItem` / `CalendarInfo` are owned Rust structs
    ///    converted at the boundary, so an invalidated `EKReminder` can never
    ///    escape into caller hands.
    ///
    /// If either changes — an `ensure_*` moved below a fetch, or a live
    /// `Retained<EKObject>` became part of the public API — this reset would
    /// start invalidating objects out from under callers.
    fn reconcile(&mut self, current: AuthorizationStatus) {
        if needs_store_reset(self.last_status, current) {
            unsafe { self.store.reset() };
        }
        self.last_status = Some(current);
    }
}

/// Should a cached store be reset, given the status it last saw and the
/// current one?
///
/// Split out so the policy is unit-testable — the effect (`EKEventStore::reset`)
/// has no observable result, so the DECISION is the only thing a test can
/// assert on. `reconcile` is the sole caller; keeping one function means the
/// test cannot drift from the behaviour.
fn needs_store_reset(previous: Option<AuthorizationStatus>, current: AuthorizationStatus) -> bool {
    match previous {
        // Unchanged — do NOT reset. `reset()` discards uncommitted changes, so
        // calling it gratuitously would drop a caller's in-flight edits.
        Some(p) if p == current => false,
        // First use on this thread: nothing has been read through the store
        // yet, so there is no stale state to clear.
        None => false,
        Some(_) => true,
    }
}

thread_local! {
    /// Per-thread reminders store. See [`StoreCache`].
    static REMINDER_STORE: std::cell::RefCell<Option<StoreCache>> =
        const { std::cell::RefCell::new(None) };
    /// Per-thread events store. Separate from the reminders one because the
    /// two managers have always held distinct stores; unifying them is a
    /// larger change than the caching fix warrants.
    static EVENT_STORE: std::cell::RefCell<Option<StoreCache>> =
        const { std::cell::RefCell::new(None) };
}

/// The main reminders manager providing access to EventKit functionality
pub struct RemindersManager {
    store: Retained<EKEventStore>,
}

impl RemindersManager {
    /// Creates a new RemindersManager instance.
    ///
    /// Cheap: the underlying `EKEventStore` is cached per thread and this only
    /// retains it (see the private `StoreCache`). Callers may keep constructing managers
    /// freely — that was the pre-existing pattern at ~68 call sites and it no
    /// longer pays EventKit initialization each time.
    pub fn new() -> Self {
        let store = REMINDER_STORE.with(|cell| {
            cell.borrow_mut()
                .get_or_insert_with(StoreCache::build)
                .store
                .clone()
        });
        Self { store }
    }

    /// Whether two managers share the same underlying `EKEventStore`.
    ///
    /// Diagnostic for the thread-local store cache — repeated construction on
    /// one thread should retain a single store rather than build a new one.
    /// Pointer identity, so it is deterministic (unlike timing).
    pub fn shares_store_with(&self, other: &Self) -> bool {
        std::ptr::eq(&*self.store, &*other.store)
    }

    /// Gets the current authorization status for reminders
    pub fn authorization_status() -> AuthorizationStatus {
        let status =
            unsafe { EKEventStore::authorizationStatusForEntityType(EKEntityType::Reminder) };
        status.into()
    }

    /// Requests full access to reminders (blocking)
    ///
    /// Returns Ok(true) if access was granted, Ok(false) if denied
    pub fn request_access(&self) -> Result<bool> {
        let result = Arc::new((Mutex::new(None::<(bool, Option<String>)>), Condvar::new()));
        let result_clone = Arc::clone(&result);

        let completion = RcBlock::new(move |granted: Bool, error: *mut NSError| {
            let error_msg = if !error.is_null() {
                let error_ref = unsafe { &*error };
                Some(format!("{:?}", error_ref))
            } else {
                None
            };

            let (lock, cvar) = &*result_clone;
            let mut res = lock.lock();
            *res = Some((granted.as_bool(), error_msg));
            cvar.notify_one();
        });

        unsafe {
            // Convert RcBlock to raw pointer for the API
            let block_ptr = &*completion as *const _ as *mut _;
            self.store
                .requestFullAccessToRemindersWithCompletion(block_ptr);
        }

        let (lock, cvar) = &*result;
        let mut res = lock.lock();
        if !wait_for_completion(cvar, &mut res, AUTHORIZATION_TIMEOUT) {
            return Err(RemindersError::AuthorizationRequestFailed(
                "timed out waiting for the system authorization response".to_string(),
            ));
        }

        match res.take() {
            Some((granted, None)) => Ok(granted),
            Some((_, Some(error))) => Err(RemindersError::AuthorizationRequestFailed(error)),
            None => Err(RemindersError::AuthorizationRequestFailed(
                "Unknown error".to_string(),
            )),
        }
    }

    /// Resolve an authorization check for `need`, prompting once if the user
    /// has not decided yet.
    ///
    /// Shared by [`ensure_full_access`](Self::ensure_full_access) and
    /// [`ensure_write_access`](Self::ensure_write_access) — the decision table
    /// lives in [`authorization_verdict`], this only adds the side effect
    /// (prompt) and the re-check.
    fn ensure_access(&self, need: AccessNeed) -> Result<()> {
        let status = Self::authorization_status();
        Self::reconcile_store(status);
        match authorization_verdict(need, status) {
            AuthVerdict::Allowed => Ok(()),
            AuthVerdict::Refused(refusal) => Err(refusal.into_error()),
            AuthVerdict::MustRequest => {
                self.request_access()?;
                // Re-check rather than trusting the `granted` bool: the user
                // may have granted a level that still doesn't cover `need`.
                // Reconciling here is the load-bearing call — this is the
                // NotDetermined → FullAccess transition, exactly the case
                // Apple says needs `reset()` before the store returns data.
                let status = Self::authorization_status();
                Self::reconcile_store(status);
                match authorization_verdict(need, status) {
                    AuthVerdict::Allowed => Ok(()),
                    AuthVerdict::Refused(refusal) => Err(refusal.into_error()),
                    // Still undecided after prompting — treat as a denial.
                    AuthVerdict::MustRequest => Err(RemindersError::AuthorizationDenied),
                }
            }
        }
    }

    /// Reset this thread's cached store if the grant changed since last use.
    fn reconcile_store(status: AuthorizationStatus) {
        REMINDER_STORE.with(|cell| {
            if let Some(cache) = cell.borrow_mut().as_mut() {
                cache.reconcile(status);
            }
        });
    }

    /// Full access — required for ANY read/list/search/update/delete path.
    ///
    /// `WriteOnly` is an ERROR here. Apple does not return reminders to a
    /// write-only client, so allowing the call would produce an empty result
    /// indistinguishable from a genuinely empty list.
    ///
    /// (Reminders has no write-only tier — only `requestFullAccessToReminders`
    /// exists — so that arm is unreachable in practice. It is kept as a
    /// defensive refusal rather than a silent `Ok`.)
    pub fn ensure_full_access(&self) -> Result<()> {
        self.ensure_access(AccessNeed::Full)
    }

    /// Create-only paths. Both `FullAccess` and `WriteOnly` satisfy this.
    pub fn ensure_write_access(&self) -> Result<()> {
        self.ensure_access(AccessNeed::Write)
    }

    /// Lists all reminder calendars (lists)
    pub fn list_calendars(&self) -> Result<Vec<CalendarInfo>> {
        self.ensure_full_access()?;

        let calendars = unsafe { self.store.calendarsForEntityType(EKEntityType::Reminder) };

        let mut result = Vec::new();
        for calendar in calendars.iter() {
            result.push(calendar_to_info(&calendar));
        }

        Ok(result)
    }

    /// Lists all available sources (iCloud, Local, Exchange, etc.)
    pub fn list_sources(&self) -> Result<Vec<SourceInfo>> {
        self.ensure_full_access()?;
        let sources = unsafe { self.store.sources() };
        let mut result = Vec::new();
        for source in sources.iter() {
            result.push(source_to_info(&source));
        }
        Ok(result)
    }

    /// Gets the default calendar for new reminders
    pub fn default_calendar(&self) -> Result<CalendarInfo> {
        self.ensure_full_access()?;

        let calendar = unsafe { self.store.defaultCalendarForNewReminders() };

        match calendar {
            Some(cal) => Ok(calendar_to_info(&cal)),
            None => Err(RemindersError::NoDefaultCalendar),
        }
    }

    /// Fetches all reminders (blocking)
    pub fn fetch_all_reminders(&self) -> Result<Vec<ReminderItem>> {
        self.fetch_reminders(None)
    }

    /// Fetches reminders from specific calendars (blocking)
    pub fn fetch_reminders(&self, calendar_titles: Option<&[&str]>) -> Result<Vec<ReminderItem>> {
        self.ensure_full_access()?;

        let calendars: Option<Retained<NSArray<EKCalendar>>> = match calendar_titles {
            Some(titles) => {
                let all_calendars =
                    unsafe { self.store.calendarsForEntityType(EKEntityType::Reminder) };
                let mut matching: Vec<Retained<EKCalendar>> = Vec::new();

                for cal in all_calendars.iter() {
                    let title = unsafe { cal.title() };
                    let title_str = title.to_string();
                    if titles.iter().any(|t| *t == title_str) {
                        matching.push(cal.retain());
                    }
                }

                if matching.is_empty() {
                    return Err(RemindersError::CalendarNotFound(titles.join(", ")));
                }

                Some(NSArray::from_retained_slice(&matching))
            }
            None => None,
        };

        let predicate = unsafe {
            self.store
                .predicateForRemindersInCalendars(calendars.as_deref())
        };

        let result = Arc::new((Mutex::new(None::<Vec<ReminderItem>>), Condvar::new()));
        let result_clone = Arc::clone(&result);

        let completion = RcBlock::new(move |reminders: *mut NSArray<EKReminder>| {
            let items = if reminders.is_null() {
                Vec::new()
            } else {
                let reminders = unsafe { Retained::retain(reminders).unwrap() };
                reminders.iter().map(|r| reminder_to_item(&r)).collect()
            };
            let (lock, cvar) = &*result_clone;
            let mut guard = lock.lock();
            *guard = Some(items);
            cvar.notify_one();
        });

        unsafe {
            self.store
                .fetchRemindersMatchingPredicate_completion(&predicate, &completion);
        }

        let (lock, cvar) = &*result;
        let mut guard = lock.lock();
        if !wait_for_completion(cvar, &mut guard, COMPLETION_TIMEOUT) {
            return Err(RemindersError::FetchFailed(
                "timed out waiting for EventKit to return reminders".to_string(),
            ));
        }

        guard
            .take()
            .ok_or_else(|| RemindersError::FetchFailed("Unknown error".to_string()))
    }

    /// Fetches incomplete reminders (no due-date filter, all calendars).
    pub fn fetch_incomplete_reminders(&self) -> Result<Vec<ReminderItem>> {
        self.fetch_incomplete_reminders_in_due_range(None, None, None)
    }

    /// Fetches incomplete reminders whose due date falls within the optional
    /// `starting`..`ending` window. Either bound may be `None` to leave it
    /// open-ended. `calendar_titles` filters to specific lists (passing
    /// `None` searches all reminder calendars).
    pub fn fetch_incomplete_reminders_in_due_range(
        &self,
        starting: Option<DateTime<Local>>,
        ending: Option<DateTime<Local>>,
        calendar_titles: Option<&[&str]>,
    ) -> Result<Vec<ReminderItem>> {
        self.ensure_full_access()?;
        let calendars = self.resolve_reminder_calendars(calendar_titles)?;
        let start_ns = starting.map(datetime_to_nsdate);
        let end_ns = ending.map(datetime_to_nsdate);
        let predicate = unsafe {
            self.store
                .predicateForIncompleteRemindersWithDueDateStarting_ending_calendars(
                    start_ns.as_deref(),
                    end_ns.as_deref(),
                    calendars.as_deref(),
                )
        };
        self.fetch_by_predicate(&predicate)
    }

    /// Fetches completed reminders whose completion date falls within the
    /// optional `starting`..`ending` window. Either bound may be `None`.
    /// `calendar_titles` filters to specific lists.
    pub fn fetch_completed_reminders_in_range(
        &self,
        starting: Option<DateTime<Local>>,
        ending: Option<DateTime<Local>>,
        calendar_titles: Option<&[&str]>,
    ) -> Result<Vec<ReminderItem>> {
        self.ensure_full_access()?;
        let calendars = self.resolve_reminder_calendars(calendar_titles)?;
        let start_ns = starting.map(datetime_to_nsdate);
        let end_ns = ending.map(datetime_to_nsdate);
        let predicate = unsafe {
            self.store
                .predicateForCompletedRemindersWithCompletionDateStarting_ending_calendars(
                    start_ns.as_deref(),
                    end_ns.as_deref(),
                    calendars.as_deref(),
                )
        };
        self.fetch_by_predicate(&predicate)
    }

    /// Resolves `calendar_titles` (`Some(&["A", "B"])` etc.) to an NSArray
    /// of `EKCalendar` references restricted to reminder calendars. `None`
    /// → `Ok(None)` meaning "all calendars". Unknown title → error.
    fn resolve_reminder_calendars(
        &self,
        calendar_titles: Option<&[&str]>,
    ) -> Result<Option<Retained<NSArray<EKCalendar>>>> {
        let Some(titles) = calendar_titles else {
            return Ok(None);
        };
        let all_calendars = unsafe { self.store.calendarsForEntityType(EKEntityType::Reminder) };
        let mut matching: Vec<Retained<EKCalendar>> = Vec::new();
        for cal in all_calendars.iter() {
            let title_str = unsafe { cal.title() }.to_string();
            if titles.iter().any(|t| *t == title_str) {
                matching.push(cal.retain());
            }
        }
        if matching.is_empty() {
            return Err(RemindersError::CalendarNotFound(titles.join(", ")));
        }
        Ok(Some(NSArray::from_retained_slice(&matching)))
    }

    /// Runs the standard block-completion fetch dance against any predicate
    /// and converts the returned EKReminders to ReminderItems. Used by all
    /// `fetch_*` methods.
    fn fetch_by_predicate(
        &self,
        predicate: &objc2_foundation::NSPredicate,
    ) -> Result<Vec<ReminderItem>> {
        let result = Arc::new((Mutex::new(None::<Vec<ReminderItem>>), Condvar::new()));
        let result_clone = Arc::clone(&result);

        let completion = RcBlock::new(move |reminders: *mut NSArray<EKReminder>| {
            let items = if reminders.is_null() {
                Vec::new()
            } else {
                let reminders = unsafe { Retained::retain(reminders).unwrap() };
                reminders.iter().map(|r| reminder_to_item(&r)).collect()
            };
            let (lock, cvar) = &*result_clone;
            let mut guard = lock.lock();
            *guard = Some(items);
            cvar.notify_one();
        });

        unsafe {
            self.store
                .fetchRemindersMatchingPredicate_completion(predicate, &completion);
        }

        let (lock, cvar) = &*result;
        let mut guard = lock.lock();
        if !wait_for_completion(cvar, &mut guard, COMPLETION_TIMEOUT) {
            return Err(RemindersError::FetchFailed(
                "timed out waiting for EventKit to return reminders".to_string(),
            ));
        }
        guard
            .take()
            .ok_or_else(|| RemindersError::FetchFailed("Unknown error".to_string()))
    }

    /// Creates a new reminder. Build the input with `ReminderDraft` —
    /// only `title` is required; spread `..Default::default()` for the rest.
    pub fn create_reminder(&self, draft: &ReminderDraft<'_>) -> Result<ReminderItem> {
        // A write-only grant DOES permit creating into the default calendar —
        // that is exactly the tier Apple designed it for. It does NOT permit
        // resolving a calendar BY TITLE, which reads the calendar list and
        // returns nothing to a write-only client. Gate on what this call
        // actually needs so write-only users keep the capability they have.
        if draft.calendar_title.is_some() {
            self.ensure_full_access()?;
        } else {
            self.ensure_write_access()?;
        }

        let reminder = unsafe { EKReminder::reminderWithEventStore(&self.store) };

        let ns_title = NSString::from_str(draft.title);
        unsafe { reminder.setTitle(Some(&ns_title)) };

        if let Some(notes_text) = draft.notes {
            let ns_notes = NSString::from_str(notes_text);
            unsafe { reminder.setNotes(Some(&ns_notes)) };
        }

        if let Some(p) = draft.priority {
            unsafe { reminder.setPriority(p) };
        }

        if let Some(due) = draft.due_date {
            let components = datetime_to_date_components(due);
            unsafe { reminder.setDueDateComponents(Some(&components)) };
        }

        if let Some(start) = draft.start_date {
            let components = datetime_to_date_components(start);
            unsafe { reminder.setStartDateComponents(Some(&components)) };
        }

        if draft.URL.is_some() {
            set_item_URL(&reminder, draft.URL)?;
        }
        if draft.location.is_some() {
            set_item_location(&reminder, draft.location);
        }
        if let Some(sl) = draft.structured_location {
            set_reminder_structured_location(&reminder, Some(sl));
        }
        if draft.due_date_timezone.is_some() {
            set_reminder_due_date_timezone(&reminder, draft.due_date_timezone);
        }

        let calendar = if let Some(cal_title) = draft.calendar_title {
            self.find_calendar_by_title(cal_title)?
        } else {
            unsafe { self.store.defaultCalendarForNewReminders() }
                .ok_or(RemindersError::NoDefaultCalendar)?
        };
        unsafe { reminder.setCalendar(Some(&calendar)) };

        self.save_reminder_and_refresh(&reminder)?;

        Ok(reminder_to_item(&reminder))
    }

    /// Updates an existing reminder. Build the changeset with `ReminderPatch`
    /// — only set the fields you want to touch. For nullable string/timezone
    /// fields, `Some(Some(v))` writes `v` and `Some(None)` clears.
    pub fn update_reminder(
        &self,
        identifier: &str,
        patch: &ReminderPatch<'_>,
    ) -> Result<ReminderItem> {
        self.ensure_full_access()?;

        let reminder = self.find_reminder_by_id(identifier)?;

        if let Some(t) = patch.title {
            let ns_title = NSString::from_str(t);
            unsafe { reminder.setTitle(Some(&ns_title)) };
        }

        if let Some(n) = patch.notes {
            let ns_notes = NSString::from_str(n);
            unsafe { reminder.setNotes(Some(&ns_notes)) };
        }

        if let Some(c) = patch.completed {
            unsafe { reminder.setCompleted(c) };
        }

        if let Some(p) = patch.priority {
            unsafe { reminder.setPriority(p) };
        }

        if let Some(due_opt) = patch.due_date {
            match due_opt {
                Some(due) => {
                    let components = datetime_to_date_components(due);
                    unsafe { reminder.setDueDateComponents(Some(&components)) };
                }
                None => unsafe { reminder.setDueDateComponents(None) },
            }
        }

        if let Some(start_opt) = patch.start_date {
            match start_opt {
                Some(start) => {
                    let components = datetime_to_date_components(start);
                    unsafe { reminder.setStartDateComponents(Some(&components)) };
                }
                None => unsafe { reminder.setStartDateComponents(None) },
            }
        }

        if let Some(url_opt) = patch.URL {
            set_item_URL(&reminder, url_opt)?;
        }
        if let Some(loc_opt) = patch.location {
            set_item_location(&reminder, loc_opt);
        }
        if let Some(sl_opt) = patch.structured_location {
            set_reminder_structured_location(&reminder, sl_opt);
        }
        if let Some(tz_opt) = patch.due_date_timezone {
            set_reminder_due_date_timezone(&reminder, tz_opt);
        }

        // Apply completion_date AFTER `completed` so Apple's "completionDate
        // is authoritative" semantics win when both are provided.
        if let Some(cd_opt) = patch.completion_date {
            match cd_opt {
                Some(d) => {
                    let nsdate = datetime_to_nsdate(d);
                    unsafe { reminder.setCompletionDate(Some(&nsdate)) };
                }
                None => unsafe { reminder.setCompletionDate(None) },
            }
        }

        if let Some(cal_title) = patch.calendar_title {
            let calendar = self.find_calendar_by_title(cal_title)?;
            unsafe { reminder.setCalendar(Some(&calendar)) };
        }

        self.save_reminder_and_refresh(&reminder)?;

        Ok(reminder_to_item(&reminder))
    }

    /// Marks a reminder as complete
    pub fn complete_reminder(&self, identifier: &str) -> Result<ReminderItem> {
        self.update_reminder(
            identifier,
            &ReminderPatch {
                completed: Some(true),
                ..Default::default()
            },
        )
    }

    /// Marks a reminder as incomplete
    pub fn uncomplete_reminder(&self, identifier: &str) -> Result<ReminderItem> {
        self.update_reminder(
            identifier,
            &ReminderPatch {
                completed: Some(false),
                ..Default::default()
            },
        )
    }

    /// Deletes a reminder
    pub fn delete_reminder(&self, identifier: &str) -> Result<()> {
        self.ensure_full_access()?;

        let reminder = self.find_reminder_by_id(identifier)?;

        unsafe {
            self.store
                .removeReminder_commit_error(&reminder, true)
                .map_err(|e| EventKitError::DeleteFailed(format!("{:?}", e)))?;
        }

        Ok(())
    }

    /// Gets a reminder by its identifier
    pub fn get_reminder(&self, identifier: &str) -> Result<ReminderItem> {
        self.ensure_full_access()?;
        let reminder = self.find_reminder_by_id(identifier)?;
        Ok(reminder_to_item(&reminder))
    }

    /// Dumps every Objective-C `@property` declared on the reminder, its
    /// EKCalendar, and that calendar's EKSource — using runtime reflection
    /// (`class_copyPropertyList`). Intended for figuring out which native
    /// fields are not yet surfaced by [`ReminderItem`].
    ///
    /// If `read_values` is true, also reads each property via KVC
    /// (`valueForKey:`). Some properties in EventKit are backed by Core Data
    /// internals and hit C-level assertions when read this way — see the
    /// hardcoded denylist in `reflect_object_full`. Use `false` for a fully
    /// safe schema-only listing.
    pub fn dump_reminder_raw(&self, identifier: &str, read_values: bool) -> Result<String> {
        self.ensure_full_access()?;
        let reminder = self.find_reminder_by_id(identifier)?;

        let mut out = String::new();
        out.push_str(&reflect_object_full(
            "EKReminder (instance)",
            &*reminder,
            read_values,
        ));

        if let Some(calendar) = unsafe { reminder.calendar() } {
            out.push('\n');
            out.push_str(&reflect_object_full(
                "EKCalendar (reminder.calendar)",
                &*calendar,
                read_values,
            ));

            if let Some(source) = unsafe { calendar.source() } {
                out.push('\n');
                out.push_str(&reflect_object_full(
                    "EKSource (calendar.source)",
                    &*source,
                    read_values,
                ));
            }
        }

        Ok(out)
    }

    /// Probes a hardcoded list of suspected-private selectors on the reminder
    /// — the kind of accessors that aren't generated by `objc2-event-kit`
    /// and reject KVC `valueForKey:` (e.g. `structuredData`, `tags`,
    /// `richLink`). Each call is wrapped in an Objective-C exception catch.
    /// Read-only — never invokes any setter.
    pub fn dump_reminder_private(&self, identifier: &str) -> Result<String> {
        self.ensure_full_access()?;
        let reminder = self.find_reminder_by_id(identifier)?;
        let obj =
            unsafe { &*(&*reminder as *const EKReminder).cast::<objc2::runtime::AnyObject>() };
        Ok(probe_private_selectors(obj, "EKReminder"))
    }

    // ========================================================================
    // Alarm Management
    // ========================================================================

    /// Lists all alarms on a reminder.
    pub fn get_alarms(&self, identifier: &str) -> Result<Vec<AlarmInfo>> {
        self.ensure_full_access()?;
        let reminder = self.find_reminder_by_id(identifier)?;
        Ok(get_item_alarms(&reminder))
    }

    /// Adds an alarm to a reminder.
    pub fn add_alarm(&self, identifier: &str, alarm: &AlarmInfo) -> Result<()> {
        self.ensure_full_access()?;
        let reminder = self.find_reminder_by_id(identifier)?;
        add_item_alarm(&reminder, alarm)?;
        self.save_reminder_and_refresh(&reminder)?;
        Ok(())
    }

    /// Removes all alarms from a reminder.
    pub fn remove_all_alarms(&self, identifier: &str) -> Result<()> {
        self.ensure_full_access()?;
        let reminder = self.find_reminder_by_id(identifier)?;
        clear_item_alarms(&reminder);
        self.save_reminder_and_refresh(&reminder)?;
        Ok(())
    }

    /// Removes a specific alarm from a reminder by index.
    pub fn remove_alarm(&self, identifier: &str, index: usize) -> Result<()> {
        self.ensure_full_access()?;
        let reminder = self.find_reminder_by_id(identifier)?;
        remove_item_alarm(&reminder, index)?;
        self.save_reminder_and_refresh(&reminder)?;
        Ok(())
    }

    // ========================================================================
    // URL Management
    // ========================================================================

    /// Set or clear the URL on a reminder.
    #[allow(non_snake_case)]
    pub fn set_URL(&self, identifier: &str, url: Option<&str>) -> Result<()> {
        self.ensure_full_access()?;
        let reminder = self.find_reminder_by_id(identifier)?;
        set_item_URL(&reminder, url)?;
        self.save_reminder_and_refresh(&reminder)?;
        Ok(())
    }

    /// Set or clear the free-text `location` on a reminder.
    pub fn set_location(&self, identifier: &str, location: Option<&str>) -> Result<()> {
        self.ensure_full_access()?;
        let reminder = self.find_reminder_by_id(identifier)?;
        set_item_location(&reminder, location);
        self.save_reminder_and_refresh(&reminder)?;
        Ok(())
    }

    /// Set or clear `EKReminder.dueDateTimeZone`. `tz_name` must be an
    /// IANA zone identifier (e.g. `"America/Los_Angeles"`).
    pub fn set_due_date_timezone(&self, identifier: &str, tz_name: Option<&str>) -> Result<()> {
        self.ensure_full_access()?;
        let reminder = self.find_reminder_by_id(identifier)?;
        set_reminder_due_date_timezone(&reminder, tz_name);
        self.save_reminder_and_refresh(&reminder)?;
        Ok(())
    }

    /// Set (or clear) the reminder's rich location — `EKReminder.structuredLocation`
    /// (not in `objc2-event-kit` 0.3 generated bindings — accessed via
    /// `msg_send!`).
    ///
    /// **iCloud caveat:** verified empirically — iCloud silently drops this
    /// mutation on reminder objects even though the save returns success.
    /// Use [`set_geofence`](Self::set_geofence) instead for iCloud-synced
    /// reminders; it writes the structured location on a proximity alarm,
    /// which iCloud does persist. This method remains correct for local /
    /// non-iCloud reminder sources.
    pub fn set_structured_location(
        &self,
        identifier: &str,
        loc: Option<&StructuredLocation>,
    ) -> Result<()> {
        self.ensure_full_access()?;
        let reminder = self.find_reminder_by_id(identifier)?;
        set_reminder_structured_location(&reminder, loc);
        self.save_reminder_and_refresh(&reminder)?;
        Ok(())
    }

    /// Attach (or clear) a geofence on the reminder. Implemented as a
    /// location-based `EKAlarm` since EventKit has no `structuredLocation`
    /// directly on `EKReminder` / `EKCalendarItem`. When `geofence` is
    /// `Some`, also requests WhenInUse location authorization so the alarm
    /// actually has permission to fire — if the user denies, the save is
    /// aborted with an authorization error.
    pub fn set_geofence(
        &self,
        identifier: &str,
        geofence: Option<(&StructuredLocation, AlarmProximity)>,
    ) -> Result<()> {
        self.ensure_full_access()?;

        // Prompt for location auth before saving a geofence. Without this we
        // would silently create a reminder whose proximity trigger can't fire.
        #[cfg(feature = "location")]
        if geofence.is_some() {
            use crate::location::{LocationAuthorizationStatus as L, LocationManager};
            let loc_mgr = LocationManager::new();
            let status = if loc_mgr.authorization_status() == L::NotDetermined {
                loc_mgr.request_when_in_use_authorization();
                // CLLocationManager delivers the result asynchronously on the
                // run loop. Pump it and wait for the user to answer the dialog
                // instead of bailing while the prompt is still on screen.
                loc_mgr.wait_for_authorization(std::time::Duration::from_secs(60))
            } else {
                loc_mgr.authorization_status()
            };
            match status {
                L::Authorized => {}
                L::Denied => return Err(EventKitError::AuthorizationDenied),
                L::Restricted => return Err(EventKitError::AuthorizationRestricted),
                L::NotDetermined => return Err(EventKitError::AuthorizationNotDetermined),
            }
        }

        let reminder = self.find_reminder_by_id(identifier)?;
        set_reminder_geofence(&reminder, geofence);
        self.save_reminder_and_refresh(&reminder)?;
        Ok(())
    }

    // ========================================================================
    // Recurrence Rule Management
    // ========================================================================

    /// Gets recurrence rules on a reminder.
    pub fn get_recurrence_rules(&self, identifier: &str) -> Result<Vec<RecurrenceRule>> {
        self.ensure_full_access()?;
        let reminder = self.find_reminder_by_id(identifier)?;
        Ok(get_item_recurrence_rules(&reminder))
    }

    /// Sets a recurrence rule on a reminder (replaces any existing rules).
    pub fn set_recurrence_rule(&self, identifier: &str, rule: &RecurrenceRule) -> Result<()> {
        self.ensure_full_access()?;
        let reminder = self.find_reminder_by_id(identifier)?;
        set_item_recurrence_rule(&reminder, rule);
        self.save_reminder_and_refresh(&reminder)?;
        Ok(())
    }

    /// Removes all recurrence rules from a reminder.
    pub fn remove_recurrence_rules(&self, identifier: &str) -> Result<()> {
        self.ensure_full_access()?;
        let reminder = self.find_reminder_by_id(identifier)?;
        clear_item_recurrence_rules(&reminder);
        self.save_reminder_and_refresh(&reminder)?;
        Ok(())
    }

    // ========================================================================
    // Calendar (Reminder List) Management
    // ========================================================================

    /// Creates a new reminder list (calendar)
    ///
    /// The list will be created in the default source (usually iCloud or Local).
    pub fn create_calendar(&self, title: &str) -> Result<CalendarInfo> {
        self.ensure_full_access()?;

        // Create a new calendar for reminders
        let calendar = unsafe {
            EKCalendar::calendarForEntityType_eventStore(EKEntityType::Reminder, &self.store)
        };

        // Set the title
        let ns_title = NSString::from_str(title);
        unsafe { calendar.setTitle(&ns_title) };

        // Find a suitable source (prefer iCloud, fall back to local)
        let source = self.find_best_source_for_reminders()?;
        unsafe { calendar.setSource(Some(&source)) };

        // Save the calendar
        unsafe {
            self.store
                .saveCalendar_commit_error(&calendar, true)
                .map_err(|e| EventKitError::SaveFailed(format!("{:?}", e)))?;
        }

        Ok(calendar_to_info(&calendar))
    }

    /// Renames an existing reminder list (calendar)
    /// Rename a reminder list (backward compat wrapper).
    pub fn rename_calendar(&self, identifier: &str, new_title: &str) -> Result<CalendarInfo> {
        self.update_calendar(identifier, Some(new_title), None)
    }

    /// Update a reminder list — name, color, or both.
    pub fn update_calendar(
        &self,
        identifier: &str,
        new_title: Option<&str>,
        color_rgba: Option<(f64, f64, f64, f64)>,
    ) -> Result<CalendarInfo> {
        self.ensure_full_access()?;
        let calendar = self.find_calendar_by_id(identifier)?;

        if !unsafe { calendar.allowsContentModifications() } {
            return Err(EventKitError::SaveFailed(
                "Calendar does not allow modifications".to_string(),
            ));
        }

        if let Some(title) = new_title {
            let ns_title = NSString::from_str(title);
            unsafe { calendar.setTitle(&ns_title) };
        }

        if let Some((r, g, b, a)) = color_rgba {
            let cg = objc2_core_graphics::CGColor::new_srgb(r, g, b, a);
            unsafe { calendar.setCGColor(Some(&cg)) };
        }

        unsafe {
            self.store
                .saveCalendar_commit_error(&calendar, true)
                .map_err(|e| EventKitError::SaveFailed(format!("{:?}", e)))?;
        }

        Ok(calendar_to_info(&calendar))
    }

    /// Deletes a reminder list (calendar)
    ///
    /// Warning: This will delete all reminders in the list!
    pub fn delete_calendar(&self, identifier: &str) -> Result<()> {
        self.ensure_full_access()?;

        let calendar = self.find_calendar_by_id(identifier)?;

        // Check if modifications are allowed
        if !unsafe { calendar.allowsContentModifications() } {
            return Err(EventKitError::DeleteFailed(
                "Calendar does not allow modifications".to_string(),
            ));
        }

        unsafe {
            self.store
                .removeCalendar_commit_error(&calendar, true)
                .map_err(|e| EventKitError::DeleteFailed(format!("{:?}", e)))?;
        }

        Ok(())
    }

    /// Gets a calendar by its identifier
    pub fn get_calendar(&self, identifier: &str) -> Result<CalendarInfo> {
        self.ensure_full_access()?;
        let calendar = self.find_calendar_by_id(identifier)?;
        Ok(calendar_to_info(&calendar))
    }

    // Helper to find the best source for creating new reminder calendars
    fn find_best_source_for_reminders(&self) -> Result<Retained<objc2_event_kit::EKSource>> {
        // Try to get the source from the default calendar first
        if let Some(default_cal) = unsafe { self.store.defaultCalendarForNewReminders() }
            && let Some(source) = unsafe { default_cal.source() }
        {
            return Ok(source);
        }

        // Fall back to finding any source that supports reminders
        let sources = unsafe { self.store.sources() };
        for source in sources.iter() {
            // Check if this source supports reminder calendars
            let calendars = unsafe { source.calendarsForEntityType(EKEntityType::Reminder) };
            if !calendars.is_empty() {
                return Ok(source.retain());
            }
        }

        Err(EventKitError::SaveFailed(
            "No suitable source found for creating reminder calendar".to_string(),
        ))
    }

    // Helper to find a calendar by identifier
    fn find_calendar_by_id(&self, identifier: &str) -> Result<Retained<EKCalendar>> {
        let ns_id = NSString::from_str(identifier);
        let calendar = unsafe { self.store.calendarWithIdentifier(&ns_id) };

        match calendar {
            Some(cal) => Ok(cal),
            None => Err(EventKitError::CalendarNotFound(identifier.to_string())),
        }
    }

    // Helper to find a calendar by title
    fn find_calendar_by_title(&self, title: &str) -> Result<Retained<EKCalendar>> {
        let calendars = unsafe { self.store.calendarsForEntityType(EKEntityType::Reminder) };

        for cal in calendars.iter() {
            let cal_title = unsafe { cal.title() };
            if cal_title.to_string() == title {
                return Ok(cal.retain());
            }
        }

        Err(RemindersError::CalendarNotFound(title.to_string()))
    }

    // Helper to find a reminder by identifier
    /// Commit a reminder to the store and refresh sources, so a subsequent
    /// `find_reminder_by_id` or fetch sees the just-saved state without
    /// waiting for the daemon to notice. All `pub fn` save paths on
    /// `RemindersManager` route through this helper.
    fn save_reminder_and_refresh(&self, reminder: &EKReminder) -> Result<()> {
        unsafe {
            self.store
                .saveReminder_commit_error(reminder, true)
                .map_err(|e| EventKitError::SaveFailed(format!("{:?}", e)))?;
            self.store.refreshSourcesIfNecessary();
        }
        Ok(())
    }

    fn find_reminder_by_id(&self, identifier: &str) -> Result<Retained<EKReminder>> {
        // Pick up any changes the user made in Reminders.app since the
        // store was created — without this, iCloud edits made on the same
        // machine may not be visible until next process launch.
        unsafe { self.store.refreshSourcesIfNecessary() };

        let ns_id = NSString::from_str(identifier);
        let item = unsafe { self.store.calendarItemWithIdentifier(&ns_id) };

        match item {
            Some(item) => {
                // Try to downcast to EKReminder
                if let Some(reminder) = item.downcast_ref::<EKReminder>() {
                    Ok(reminder.retain())
                } else {
                    Err(EventKitError::ItemNotFound(identifier.to_string()))
                }
            }
            None => Err(EventKitError::ItemNotFound(identifier.to_string())),
        }
    }
}

impl Default for RemindersManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Authorization status for reminders access
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthorizationStatus {
    /// User has not yet made a choice
    NotDetermined,
    /// Access restricted by system policy
    Restricted,
    /// User explicitly denied access
    Denied,
    /// Full access granted
    FullAccess,
    /// Write-only access granted
    WriteOnly,
}

impl From<EKAuthorizationStatus> for AuthorizationStatus {
    fn from(status: EKAuthorizationStatus) -> Self {
        if status == EKAuthorizationStatus::NotDetermined {
            AuthorizationStatus::NotDetermined
        } else if status == EKAuthorizationStatus::Restricted {
            AuthorizationStatus::Restricted
        } else if status == EKAuthorizationStatus::Denied {
            AuthorizationStatus::Denied
        } else if status == EKAuthorizationStatus::FullAccess {
            AuthorizationStatus::FullAccess
        } else if status == EKAuthorizationStatus::WriteOnly {
            AuthorizationStatus::WriteOnly
        } else {
            AuthorizationStatus::NotDetermined
        }
    }
}

impl std::fmt::Display for AuthorizationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuthorizationStatus::NotDetermined => write!(f, "Not Determined"),
            AuthorizationStatus::Restricted => write!(f, "Restricted"),
            AuthorizationStatus::Denied => write!(f, "Denied"),
            AuthorizationStatus::FullAccess => write!(f, "Full Access"),
            AuthorizationStatus::WriteOnly => write!(f, "Write Only"),
        }
    }
}

/// The access level an operation needs.
///
/// Apple does not offer a read-only tier: *"Your app can't request read-only
/// access to either events or reminders. To read events or reminders from the
/// event store, your app needs full access."* So every read/list/search/update/
/// delete path is [`AccessNeed::Full`], and only genuine create-only paths can
/// settle for [`AccessNeed::Write`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessNeed {
    /// Any path that READS the store — list, get, search, update, delete.
    Full,
    /// Create-only paths, which a write-only grant satisfies.
    Write,
}

/// Why an authorization check refused.
///
/// Distinguished because the REMEDY differs: a write-only user must grant full
/// access in System Settings, which is a different instruction from "you denied
/// us". Collapsing these into one error is what makes the failure unactionable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthRefusal {
    /// The user explicitly denied access.
    Denied,
    /// System policy (MDM, Screen Time) forbids access.
    Restricted,
    /// Write-only granted, but the operation needs to READ.
    WriteOnly,
}

/// The conclusion of an authorization check, BEFORE any side effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthVerdict {
    /// The current grant covers this operation — proceed.
    Allowed,
    /// Undecided; the caller must prompt the user, then re-check.
    MustRequest,
    /// Refuse, with the reason the caller maps to an error.
    Refused(AuthRefusal),
}

/// Decide whether `status` permits an operation needing `need`.
///
/// **This is the testability seam.**
/// `EKEventStore::authorizationStatusForEntityType` is a CLASS method reading
/// process-global TCC state, so a test that goes through it takes a different
/// branch on a machine with Calendar granted than it does on CI. Taking the
/// status as a PARAMETER lets tests drive all five values with no EventKit, no
/// store construction, and no dependence on the host's privacy settings.
///
/// The load-bearing case is `WriteOnly` + [`AccessNeed::Full`]. Apple:
/// *"Your app can create events, but it can't access any of the existing
/// calendars and events on the device, including events your app created. API
/// calls to read event data from the event store don't return any events."*
/// Returning `Allowed` there does not fail — it succeeds and returns nothing,
/// so the caller reports an empty calendar that is indistinguishable from a
/// genuinely empty one. That is why this refuses instead.
pub fn authorization_verdict(need: AccessNeed, status: AuthorizationStatus) -> AuthVerdict {
    match (need, status) {
        (_, AuthorizationStatus::FullAccess) => AuthVerdict::Allowed,
        (AccessNeed::Write, AuthorizationStatus::WriteOnly) => AuthVerdict::Allowed,
        (AccessNeed::Full, AuthorizationStatus::WriteOnly) => {
            AuthVerdict::Refused(AuthRefusal::WriteOnly)
        }
        (_, AuthorizationStatus::NotDetermined) => AuthVerdict::MustRequest,
        (_, AuthorizationStatus::Denied) => AuthVerdict::Refused(AuthRefusal::Denied),
        (_, AuthorizationStatus::Restricted) => AuthVerdict::Refused(AuthRefusal::Restricted),
    }
}

impl AuthRefusal {
    /// Map a refusal to the error the caller returns.
    fn into_error(self) -> EventKitError {
        match self {
            AuthRefusal::Denied => EventKitError::AuthorizationDenied,
            AuthRefusal::Restricted => EventKitError::AuthorizationRestricted,
            AuthRefusal::WriteOnly => EventKitError::AuthorizationWriteOnly,
        }
    }
}

// Helper function to convert EKReminder to ReminderItem
/// Walks the class chain of `obj` and lists every declared `@property`,
/// reading each one via KVC (`-[NSObject valueForKey:]`). The returned string
/// is multi-section human-readable text — one section per class in the chain.
///
/// Only intended for diagnostics: KVC dispatch is slower than direct method
/// calls and may throw NSException for very unusual property declarations.
// Properties whose KVC read path hits a C-level assertion (CADObjectID etc.)
// and aborts the process. Skip these unconditionally when `read_values` is on.
// Discovered empirically — add to this list when a new abort is found.
const REFLECT_VALUE_DENYLIST: &[&str] =
    &["objectID", "objectIDURIRepresentation", "URIRepresentation"];

fn reflect_object_full<T: Message>(header: &str, obj: &T, read_values: bool) -> String {
    use objc2::ffi::{class_copyPropertyList, free, property_getAttributes, property_getName};
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, AnyObject};
    use std::ffi::CStr;

    // Cast to AnyObject for runtime introspection. `T: Message` guarantees
    // this is an Objective-C object with a valid isa pointer.
    let obj: &AnyObject = unsafe { &*(obj as *const T as *const AnyObject) };

    let mut out = String::new();
    out.push_str(&format!("=== {header} ===\n"));

    // Build the class chain (most-derived first).
    let mut chain: Vec<&'static AnyClass> = Vec::new();
    let mut current = Some(obj.class());
    while let Some(cls) = current {
        chain.push(cls);
        current = cls.superclass();
    }
    out.push_str("Class chain: ");
    out.push_str(
        &chain
            .iter()
            .map(|c| c.name().to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" -> "),
    );
    out.push_str("\n\n");

    for cls in &chain {
        let cls_name = cls.name().to_string_lossy().into_owned();
        // Stop at NSObject — its properties are noise (hash, description, ...).
        if cls_name == "NSObject" {
            continue;
        }

        let mut count: u32 = 0;
        let props_ptr =
            unsafe { class_copyPropertyList(*cls as *const AnyClass, &mut count as *mut u32) };
        if props_ptr.is_null() || count == 0 {
            if !props_ptr.is_null() {
                unsafe { free(props_ptr as *mut _) };
            }
            continue;
        }

        let mut entries: Vec<(String, String, String)> = Vec::new();
        for i in 0..count as isize {
            let prop = unsafe { *props_ptr.offset(i) };
            if prop.is_null() {
                continue;
            }
            let name_ptr = unsafe { property_getName(prop) };
            let attrs_ptr = unsafe { property_getAttributes(prop) };
            let name = if name_ptr.is_null() {
                String::from("<?>")
            } else {
                unsafe { CStr::from_ptr(name_ptr) }
                    .to_string_lossy()
                    .into_owned()
            };
            let attrs = if attrs_ptr.is_null() {
                String::new()
            } else {
                unsafe { CStr::from_ptr(attrs_ptr) }
                    .to_string_lossy()
                    .into_owned()
            };

            // Anything typed `EKObjectID` / `NSManagedObjectID` / `CADObjectID`
            // hits a CoreData `__assert_rtn` on KVC read and will SIGABRT.
            // Filter by type encoding rather than name — safer as new
            // properties are added in future SDKs.
            let type_is_objectid = attrs.contains("ObjectID");

            let value_str = if !read_values {
                String::from("<value read skipped (pass --values to attempt)>")
            } else if REFLECT_VALUE_DENYLIST.contains(&name.as_str()) || type_is_objectid {
                String::from("<skipped: type/name denylisted (would abort process)>")
            } else {
                // Read via KVC, catching any NSException. Note: this catches
                // ObjC exceptions but NOT C-level assertions (`__assert_rtn`),
                // which abort the process directly — hence the denylist above.
                let name_for_closure = name.clone();
                let obj_ptr = obj as *const AnyObject;
                match objc2::exception::catch(std::panic::AssertUnwindSafe(move || {
                    let key = NSString::from_str(&name_for_closure);
                    let obj_ref: &AnyObject = unsafe { &*obj_ptr };
                    let value_obj: Option<Retained<AnyObject>> =
                        unsafe { msg_send![obj_ref, valueForKey: &*key] };
                    match value_obj {
                        None => String::from("(null)"),
                        Some(v) => describe_object(&v),
                    }
                })) {
                    Ok(s) => s,
                    Err(Some(exc)) => {
                        format!("<NSException: {}>", describe_object(&**exc as &AnyObject))
                    }
                    Err(None) => String::from("<unknown ObjC exception>"),
                }
            };

            entries.push((name, attrs, value_str));
        }
        unsafe { free(props_ptr as *mut _) };

        if entries.is_empty() {
            continue;
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));

        out.push_str(&format!("[{cls_name}]\n"));
        for (name, attrs, value) in entries {
            out.push_str(&format!("  {name}\n"));
            out.push_str(&format!("    attrs: {attrs}\n"));
            // Indent multi-line values so they don't break the layout.
            let indented = value.replace('\n', "\n      ");
            out.push_str(&format!("    value: {indented}\n"));
        }
        out.push('\n');
    }

    out
}

/// Probes a curated list of selectors that are not generated by
/// `objc2-event-kit` and that KVC rejects on `EKReminder`. Each call is
/// wrapped in `objc2::exception::catch` so a missing selector ("unrecognized
/// selector sent to instance") becomes a printable line instead of an abort.
///
/// This is a fishing expedition — the selector names are educated guesses
/// based on Apple's public naming patterns and what Reminders.app surfaces.
/// Add entries as new candidates come up; the cost of an extra probe is one
/// caught NSException.
/// Probe an `EKCalendarItem` subclass (reminder OR event) for undocumented
/// selectors, reporting each result's Objective-C CLASS.
///
/// `label` names the class in the report header. The class of each returned
/// value is what makes this useful: if a rich-notes property existed, its
/// value would come back as `NSAttributedString` / `NSConcreteAttributedString`
/// rather than `__NSCFString`.
fn probe_private_selectors(item: &objc2::runtime::AnyObject, label: &str) -> String {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, Sel};
    use std::ffi::CString;

    const PROBES: &[&str] = &[
        // ── Rich / attributed NOTES candidates ───────────────────────────
        //
        // The PUBLIC API has no rich-text notes: Apple's "Attaching Notes"
        // topic on EKCalendarItem is exactly two members (`notes`, `hasNotes`)
        // and `notes` is a plain `NSString`. A search of EventKit AND
        // EventKitUI for NSAttributedString/RTF returns nothing. These probes
        // exist to test whether a PRIVATE one is hiding behind the frozen
        // wrapper or on the backing persistent object.
        //
        // Read the reported [ClassName]: a plain-text field comes back as
        // `__NSCFString`/`NSTaggedPointerString`. Anything answering with
        // `NSAttributedString`, `NSConcreteAttributedString`, or `NSData`
        // holding RTF would be the find.
        "notes", // baseline — confirms what the PUBLIC field's class is
        "attributedNotes",
        "notesAttributed",
        "attributedDescription",
        "attributedTitle",
        "richNotes",
        "styledNotes",
        "formattedNotes",
        "markedUpNotes",
        "notesRTF",
        "notesRTFData",
        "rtfNotes",
        "notesData",
        "notesHTML",
        "htmlNotes",
        "bodyAttributedString",
        // Rich-link / preview URL candidates
        "appLink",      // <- confirmed exists on EKCalendarItem (method dump)
        "URLString",    // <- confirmed exists on EKCalendarItem
        "externalData", // <- confirmed exists on EKCalendarItem
        "externalModificationTag",
        "richLink",
        "richLinkData",
        "URLWithPreview",
        "urlPreview",
        "linkPreview",
        "siriSuggestedURL",
        "displayURL",
        "flaggedURL",
        "_richLink",
        "_url",
        "primaryLink",
        // Structured-location candidates on the reminder itself
        "structuredLocation",
        "geoLocation",
        "place",
        "placemark",
        // Tag candidates
        "tags",
        "tagDictionaries",
        "tagNames",
        "userTags",
        "_tags",
        "_tagDictionaries",
        "hashtags",
        // Backing structured-data blobs (KVC-blocked, try typed accessor)
        "structuredData",
        "localStructuredData",
        "_structuredData",
        // "Melted" (mutable) vs frozen reminder layer
        "meltedObject",
        "unfrozen",
        "unfrozenObject",
        "writableInstance",
        // Misc EventKit private surfaces
        "flags",
        "flags2",
        "isFlagged",
        "image",
        "imageData",
        "thumbnail",
        "color",
        "creatorBundleID",
        // Subtask / parent
        "parentReminder",
        "subreminders",
        "subtasks",
        "childReminders",
    ];

    let obj: &AnyObject = item;
    let mut out = format!("=== Private-selector probe on {label} ===\n");
    out.push_str("(each call wrapped in @try/@catch — caught exceptions mean the selector doesn't exist on this class)\n\n");

    for name in PROBES {
        let c_name = CString::new(*name).expect("selector name has no NUL");
        let sel = Sel::register(&c_name);
        let name_owned = name.to_string();
        let obj_ptr = obj as *const AnyObject;
        let line = match objc2::exception::catch(std::panic::AssertUnwindSafe(move || {
            // Confirm the object responds before we send, so we get a clean
            // "doesn't respond" instead of an NSException for the common case.
            let obj_ref: &AnyObject = unsafe { &*obj_ptr };
            let responds: bool = unsafe { msg_send![obj_ref, respondsToSelector: sel] };
            if !responds {
                return format!("{name_owned}: <does not respond>");
            }
            let result: Option<Retained<AnyObject>> =
                unsafe { msg_send![obj_ref, performSelector: sel] };
            match result {
                None => format!("{name_owned}: (null)"),
                Some(v) => {
                    let cls: &objc2::runtime::AnyClass = (*v).class();
                    let cls_name = cls.name().to_string_lossy().into_owned();
                    let desc = describe_object(&v);
                    let one_line = desc.replace('\n', "\\n");
                    format!("{name_owned}: [{cls_name}] {one_line}")
                }
            }
        })) {
            Ok(s) => s,
            Err(Some(exc)) => format!(
                "{name}: <NSException: {}>",
                describe_object(&**exc as &AnyObject)
            ),
            Err(None) => format!("{name}: <unknown ObjC exception>"),
        };
        out.push_str(&line);
        out.push('\n');
    }

    // Drill into the underlying persistent object — `backingObject` is the
    // EKPersistentObject the frozen wrapper proxies. If a method is
    // KVC-blocked on the frozen wrapper it may work on the backing object.
    let backing: Option<Retained<AnyObject>> = unsafe { msg_send![obj, backingObject] };
    if let Some(backing) = backing {
        let bcls = (*backing).class().name().to_string_lossy().into_owned();
        out.push_str(&format!("\n--- backingObject ({bcls}) probe ---\n"));
        for name in PROBES {
            let c_name = std::ffi::CString::new(*name).expect("no NUL");
            let sel = objc2::runtime::Sel::register(&c_name);
            let name_owned = name.to_string();
            let backing_ptr = &*backing as *const AnyObject;
            let line = match objc2::exception::catch(std::panic::AssertUnwindSafe(move || {
                let bref: &AnyObject = unsafe { &*backing_ptr };
                let responds: bool = unsafe { msg_send![bref, respondsToSelector: sel] };
                if !responds {
                    return format!("{name_owned}: <does not respond>");
                }
                let result: Option<Retained<AnyObject>> =
                    unsafe { msg_send![bref, performSelector: sel] };
                match result {
                    None => format!("{name_owned}: (null)"),
                    Some(v) => {
                        let cls = (*v).class().name().to_string_lossy().into_owned();
                        format!(
                            "{name_owned}: [{cls}] {}",
                            describe_object(&v).replace('\n', "\\n")
                        )
                    }
                }
            })) {
                Ok(s) => s,
                Err(Some(exc)) => format!(
                    "{name}: <NSException: {}>",
                    describe_object(&**exc as &AnyObject)
                ),
                Err(None) => format!("{name}: <unknown ObjC exception>"),
            };
            out.push_str(&line);
            out.push('\n');
        }
    }

    // Bonus: enumerate every instance method via the runtime, filtered to
    // names that look URL/tag/link/data-related. This catches selectors we
    // didn't guess in PROBES.
    out.push_str(
        "\n--- All zero-arg instance methods on EKReminder containing url/tag/link/data ---\n",
    );
    out.push_str(&list_interesting_methods(obj.class()));
    out
}

/// Lists every instance method on `cls` (walking up the class chain to EKObject)
/// whose selector name contains `url`, `tag`, `link`, `data`, `flag`, `image`,
/// `preview`, or `attachment`. Doesn't call them — just shows what exists.
fn list_interesting_methods(start: &objc2::runtime::AnyClass) -> String {
    use objc2::ffi::{class_copyMethodList, free, method_getName};
    use objc2::runtime::AnyClass;
    use std::ffi::CStr;

    let needles = [
        "url",
        "tag",
        "link",
        "data",
        "flag",
        "image",
        "preview",
        "attachment",
        "rich",
        "location",
        "structured",
        "place",
        "address",
        "geo",
    ];
    let mut hits: Vec<(String, String)> = Vec::new();

    let mut current: Option<&AnyClass> = Some(start);
    while let Some(cls) = current {
        let cls_name = cls.name().to_string_lossy().into_owned();
        if cls_name == "NSObject" {
            break;
        }
        let mut count: u32 = 0;
        let ptr = unsafe { class_copyMethodList(cls as *const AnyClass, &mut count as *mut u32) };
        if !ptr.is_null() {
            for i in 0..count as isize {
                let m = unsafe { *ptr.offset(i) };
                if m.is_null() {
                    continue;
                }
                let Some(sel) = (unsafe { method_getName(m) }) else {
                    continue;
                };
                let name_cstr_ptr = unsafe { objc2::ffi::sel_getName(sel) };
                if name_cstr_ptr.is_null() {
                    continue;
                }
                let name = unsafe { CStr::from_ptr(name_cstr_ptr) }
                    .to_string_lossy()
                    .into_owned();
                let lower = name.to_lowercase();
                if needles.iter().any(|n| lower.contains(n)) {
                    hits.push((cls_name.clone(), name));
                }
            }
            unsafe { free(ptr as *mut _) };
        }
        current = cls.superclass();
    }
    hits.sort();
    hits.dedup();
    if hits.is_empty() {
        return String::from("(none)\n");
    }
    let mut out = String::new();
    for (cls, sel) in hits {
        out.push_str(&format!("  [{cls}] {sel}\n"));
    }
    out
}

/// `-[NSObject description]` as a Rust string. Returns `<nil>` for null,
/// `<no description>` if the object doesn't respond to description.
fn describe_object(obj: &objc2::runtime::AnyObject) -> String {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;

    let desc: Option<Retained<AnyObject>> = unsafe { msg_send![obj, description] };
    match desc {
        None => String::from("<no description>"),
        Some(d) => {
            // description always returns NSString — cast pointer-wise.
            let any: &AnyObject = &d;
            let ns: &NSString = unsafe { &*(any as *const AnyObject as *const NSString) };
            ns.to_string()
        }
    }
}

fn reminder_to_item(reminder: &EKReminder) -> ReminderItem {
    let identifier = unsafe { reminder.calendarItemIdentifier() }.to_string();
    let title = unsafe { reminder.title() }.to_string();
    let notes = unsafe { reminder.notes() }.map(|n| n.to_string());
    let completed = unsafe { reminder.isCompleted() };
    let priority = unsafe { reminder.priority() };
    let cal = unsafe { reminder.calendar() };
    let calendar_title = cal.as_ref().map(|c| unsafe { c.title() }.to_string());
    let calendar_id = cal
        .as_ref()
        .map(|c| unsafe { c.calendarIdentifier() }.to_string());

    // Extract due date from dueDateComponents
    let due_date = unsafe { reminder.dueDateComponents() }
        .and_then(|components| date_components_to_datetime(&components));

    // Extract start date from startDateComponents
    let start_date = unsafe { reminder.startDateComponents() }
        .and_then(|components| date_components_to_datetime(&components));

    // Extract completion date
    let completion_date =
        unsafe { reminder.completionDate() }.map(|date| nsdate_to_datetime(&date));

    // Extract additional fields from EKCalendarItem parent class
    let external_identifier =
        unsafe { reminder.calendarItemExternalIdentifier() }.map(|id| id.to_string());
    let location = unsafe { reminder.location() }.map(|loc| loc.to_string());
    #[allow(non_snake_case)]
    let URL = unsafe { reminder.URL() }
        .as_ref()
        .and_then(|url_ref| url_ref.absoluteString())
        .map(|abs_str| abs_str.to_string());
    let creation_date = unsafe { reminder.creationDate() }.map(|date| nsdate_to_datetime(&date));
    let last_modified_date =
        unsafe { reminder.lastModifiedDate() }.map(|date| nsdate_to_datetime(&date));
    let timezone = unsafe { reminder.timeZone() }.map(|tz| tz.name().to_string());
    let due_date_timezone = get_reminder_due_date_timezone(reminder);
    let structured_location = get_reminder_structured_location(reminder);
    let parent_id = get_reminder_parent_id(reminder);
    let attachments_count = get_item_attachments_count(reminder);
    let has_alarms = unsafe { reminder.hasAlarms() };
    let has_recurrence_rules = unsafe { reminder.hasRecurrenceRules() };
    let has_attendees = unsafe { reminder.hasAttendees() };
    let has_notes = unsafe { reminder.hasNotes() };

    ReminderItem {
        identifier,
        title,
        notes,
        completed,
        priority,
        calendar_title,
        calendar_id,
        due_date,
        start_date,
        completion_date,
        external_identifier,
        location,
        URL,
        creation_date,
        last_modified_date,
        timezone,
        due_date_timezone,
        structured_location,
        parent_id,
        attachments_count,
        has_alarms,
        has_recurrence_rules,
        has_attendees,
        has_notes,
        attendees: get_item_attendees(reminder),
    }
}

/// Read `EKReminder.dueDateTimeZone` via msg_send — not exposed in
/// `objc2-event-kit` 0.3 generated bindings.
fn get_reminder_due_date_timezone(reminder: &EKReminder) -> Option<String> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    let tz: Option<Retained<objc2_foundation::NSTimeZone>> =
        unsafe { msg_send![reminder, dueDateTimeZone] };
    tz.map(|tz| tz.name().to_string())
}

/// Reads `EKReminder.structuredLocation` — the iCloud-native location chip
/// that Reminders.app displays. Apple shipped this on `EKCalendarItem` but
/// `objc2-event-kit` 0.3 doesn't generate bindings for it on reminders
/// (only on `EKEvent`/`EKAlarm`), so we go through `msg_send!`.
///
/// Falls back to the alarm-based geofence path for older reminders whose
/// location is only attached via a proximity alarm.
fn get_reminder_structured_location(reminder: &EKReminder) -> Option<StructuredLocation> {
    if let Some(sl) = direct_structured_location(reminder) {
        return Some(sl);
    }
    // Legacy fallback: scan alarms for proximity-bearing structured location.
    let alarms = unsafe { reminder.alarms() }?;
    for i in 0..alarms.len() {
        let alarm = alarms.objectAtIndex(i);
        let prox = unsafe { alarm.proximity() };
        if prox == EKAlarmProximity::None {
            continue;
        }
        let Some(structured) = (unsafe { alarm.structuredLocation() }) else {
            continue;
        };
        return Some(structured_to_plain(&structured));
    }
    None
}

fn direct_structured_location(reminder: &EKReminder) -> Option<StructuredLocation> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    let sl: Option<Retained<EKStructuredLocation>> =
        unsafe { msg_send![reminder, structuredLocation] };
    sl.map(|s| structured_to_plain(&s))
}

fn structured_to_plain(structured: &EKStructuredLocation) -> StructuredLocation {
    let title = unsafe { structured.title() }
        .map(|t| t.to_string())
        .unwrap_or_default();
    let radius = unsafe { structured.radius() };
    let (latitude, longitude) = read_geo_location_coords(structured);
    StructuredLocation {
        title,
        latitude,
        longitude,
        radius,
    }
}

/// Pull lat/lng off an `EKStructuredLocation`'s `geoLocation` (CLLocation).
/// Returns (0.0, 0.0) when the `location` feature is disabled — coordinates
/// require CoreLocation bindings.
#[cfg(feature = "location")]
fn read_geo_location_coords(structured: &EKStructuredLocation) -> (f64, f64) {
    let cl = unsafe { structured.geoLocation() };
    let Some(cl) = cl else { return (0.0, 0.0) };
    let coord = unsafe { cl.coordinate() };
    (coord.latitude, coord.longitude)
}

#[cfg(not(feature = "location"))]
fn read_geo_location_coords(_structured: &EKStructuredLocation) -> (f64, f64) {
    (0.0, 0.0)
}

/// Read `EKReminder.parentID` via KVC. The underlying type is the private
/// `EKObjectID`; we stringify via `-[description]`. Returns `None` if the
/// reminder has no parent or if the KVC read throws.
fn get_reminder_parent_id(reminder: &EKReminder) -> Option<String> {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    let reminder_obj: &AnyObject = unsafe { &*(reminder as *const EKReminder as *const AnyObject) };
    let reminder_ptr = reminder_obj as *const AnyObject;
    let value = objc2::exception::catch(std::panic::AssertUnwindSafe(move || {
        let key = NSString::from_str("parentID");
        let obj_ref: &AnyObject = unsafe { &*reminder_ptr };
        let v: Option<Retained<AnyObject>> = unsafe { msg_send![obj_ref, valueForKey: &*key] };
        v
    }))
    .ok()
    .flatten()?;
    let desc: Retained<AnyObject> = unsafe { msg_send![&*value, description] };
    let ns: &NSString = unsafe { &*(&*desc as *const AnyObject as *const NSString) };
    Some(ns.to_string())
}

/// Number of file attachments on the item. EventKit doesn't expose the
/// attachment objects via the public Rust bindings; we just count them.
fn get_item_attachments_count(item: &EKCalendarItem) -> usize {
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2_foundation::NSArray;
    let attachments: Option<Retained<NSArray>> = unsafe { msg_send![item, attachments] };
    attachments.map(|a| a.len()).unwrap_or(0)
}

// Helper function to convert EKCalendar to CalendarInfo
fn source_to_info(source: &EKSource) -> SourceInfo {
    let identifier = unsafe { source.sourceIdentifier() }.to_string();
    let title = unsafe { source.title() }.to_string();
    // EKSourceType: 0=Local, 1=Exchange, 2=CalDAV, 3=MobileMe, 4=Subscribed, 5=Birthdays
    let source_type = unsafe { source.sourceType() };
    let source_type = match source_type.0 {
        0 => "local",
        1 => "exchange",
        2 => "caldav",
        3 => "mobileme",
        4 => "subscribed",
        5 => "birthdays",
        _ => "unknown",
    }
    .to_string();

    SourceInfo {
        identifier,
        title,
        source_type,
    }
}

fn calendar_to_info(calendar: &EKCalendar) -> CalendarInfo {
    let identifier = unsafe { calendar.calendarIdentifier() }.to_string();
    let title = unsafe { calendar.title() }.to_string();
    let source = unsafe { calendar.source() }.map(|s| unsafe { s.title() }.to_string());
    let source_id =
        unsafe { calendar.source() }.map(|s| unsafe { s.sourceIdentifier() }.to_string());
    let allows_modifications = unsafe { calendar.allowsContentModifications() };
    let is_immutable = unsafe { calendar.isImmutable() };
    let is_subscribed = unsafe { calendar.isSubscribed() };

    // Calendar type: Local=0, CalDAV=1, Exchange=2, Subscription=3, Birthday=4
    let cal_type = unsafe { calendar.r#type() };
    let calendar_type = match cal_type.0 {
        0 => CalendarType::Local,
        1 => CalendarType::CalDAV,
        2 => CalendarType::Exchange,
        3 => CalendarType::Subscription,
        4 => CalendarType::Birthday,
        _ => CalendarType::Unknown,
    };

    // Read RGBA from CGColor
    let color: Option<(f64, f64, f64, f64)> = unsafe {
        calendar.CGColor().and_then(|cg| {
            use objc2_core_graphics::CGColor as CG;
            let n = CG::number_of_components(Some(&cg));
            if n >= 3 {
                let ptr = CG::components(Some(&cg));
                let r = *ptr;
                let g = *ptr.add(1);
                let b = *ptr.add(2);
                let a = if n >= 4 { *ptr.add(3) } else { 1.0 };
                Some((r, g, b, a))
            } else {
                None
            }
        })
    };

    // Allowed entity types
    let entity_mask = unsafe { calendar.allowedEntityTypes() };
    let mut allowed_entity_types = Vec::new();
    if entity_mask.0 & 1 != 0 {
        allowed_entity_types.push("event".to_string());
    }
    if entity_mask.0 & 2 != 0 {
        allowed_entity_types.push("reminder".to_string());
    }

    // EKCalendarEventAvailabilityMask: bitfield of which availability values
    // this calendar accepts on events. Bits: Busy=1, Free=2, Tentative=4,
    // Unavailable=8. Empty Vec = `EKCalendarEventAvailabilityNone`.
    let avail_mask = unsafe { calendar.supportedEventAvailabilities() };
    let mut supported_event_availabilities = Vec::new();
    if avail_mask.contains(EKCalendarEventAvailabilityMask::Busy) {
        supported_event_availabilities.push("busy".to_string());
    }
    if avail_mask.contains(EKCalendarEventAvailabilityMask::Free) {
        supported_event_availabilities.push("free".to_string());
    }
    if avail_mask.contains(EKCalendarEventAvailabilityMask::Tentative) {
        supported_event_availabilities.push("tentative".to_string());
    }
    if avail_mask.contains(EKCalendarEventAvailabilityMask::Unavailable) {
        supported_event_availabilities.push("unavailable".to_string());
    }

    CalendarInfo {
        identifier,
        title,
        source,
        source_id,
        calendar_type,
        allows_modifications,
        is_immutable,
        is_subscribed,
        color,
        allowed_entity_types,
        supported_event_availabilities,
    }
}

// ============================================================================
// Calendar Events Support
// ============================================================================

/// Event availability for scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EventAvailability {
    NotSupported,
    #[default]
    Busy,
    Free,
    Tentative,
    Unavailable,
}

impl EventAvailability {
    /// Convert to the raw `EKEventAvailability` for writing.
    pub fn to_ek(self) -> EKEventAvailability {
        match self {
            Self::NotSupported => EKEventAvailability::NotSupported,
            Self::Busy => EKEventAvailability::Busy,
            Self::Free => EKEventAvailability::Free,
            Self::Tentative => EKEventAvailability::Tentative,
            Self::Unavailable => EKEventAvailability::Unavailable,
        }
    }

    /// Map an `EKEventAvailability` back to our enum. Unknown raw values
    /// (future Apple additions) fall back to `NotSupported`.
    pub fn from_ek(raw: EKEventAvailability) -> Self {
        match raw {
            EKEventAvailability::NotSupported => Self::NotSupported,
            EKEventAvailability::Busy => Self::Busy,
            EKEventAvailability::Free => Self::Free,
            EKEventAvailability::Tentative => Self::Tentative,
            EKEventAvailability::Unavailable => Self::Unavailable,
            _ => Self::NotSupported,
        }
    }
}

/// Event status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EventStatus {
    #[default]
    None,
    Confirmed,
    Tentative,
    Canceled,
}

impl EventStatus {
    pub fn from_ek(raw: EKEventStatus) -> Self {
        match raw {
            EKEventStatus::None => Self::None,
            EKEventStatus::Confirmed => Self::Confirmed,
            EKEventStatus::Tentative => Self::Tentative,
            EKEventStatus::Canceled => Self::Canceled,
            _ => Self::None,
        }
    }
}

/// Scope of a recurring-event edit or delete — mirrors Apple's `EKSpan`.
///
/// - `This`: only the specific occurrence you have a reference to.
/// - `Future`: this occurrence and every later occurrence in the same series.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EventSpan {
    #[default]
    This,
    Future,
}

impl EventSpan {
    pub(crate) fn to_ek(self) -> EKSpan {
        match self {
            Self::This => EKSpan::ThisEvent,
            Self::Future => EKSpan::FutureEvents,
        }
    }
}

/// Participant role in an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticipantRole {
    Unknown,
    Required,
    Optional,
    Chair,
    NonParticipant,
}

/// Participant RSVP status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParticipantStatus {
    Unknown,
    Pending,
    Accepted,
    Declined,
    Tentative,
    Delegated,
    Completed,
    InProcess,
}

/// A participant (attendee) on an event or reminder.
#[derive(Debug, Clone)]
#[allow(non_snake_case)]
pub struct ParticipantInfo {
    pub name: Option<String>,
    pub URL: Option<String>,
    pub role: ParticipantRole,
    pub status: ParticipantStatus,
    pub is_current_user: bool,
}

/// Represents a calendar event with its properties.
#[derive(Debug, Clone)]
#[allow(non_snake_case)]
pub struct EventItem {
    /// Unique identifier for the event
    pub identifier: String,
    /// Title of the event
    pub title: String,
    /// Optional notes/description
    pub notes: Option<String>,
    /// Optional location (string)
    pub location: Option<String>,
    /// Start date/time
    pub start_date: DateTime<Local>,
    /// End date/time
    pub end_date: DateTime<Local>,
    /// Whether this is an all-day event
    pub all_day: bool,
    /// Calendar the event belongs to
    pub calendar_title: Option<String>,
    /// Calendar identifier
    pub calendar_id: Option<String>,
    /// URL associated with the event
    #[allow(non_snake_case)]
    pub URL: Option<String>,
    /// Availability for scheduling
    pub availability: EventAvailability,
    /// Event status (read-only)
    pub status: EventStatus,
    /// Whether this occurrence was modified from its recurring series
    pub is_detached: bool,
    /// Original date in a recurring series
    pub occurrence_date: Option<DateTime<Local>>,
    /// Geo-coordinate location
    pub structured_location: Option<StructuredLocation>,
    /// Date the event was first created (`EKCalendarItem.creationDate`).
    pub creation_date: Option<DateTime<Local>>,
    /// Date the event was last modified (`EKCalendarItem.lastModifiedDate`).
    pub last_modified_date: Option<DateTime<Local>>,
    /// Server-provided external identifier (`EKCalendarItem.calendarItemExternalIdentifier`).
    pub external_identifier: Option<String>,
    /// Item-level timezone (`EKCalendarItem.timeZone`). Distinct from the
    /// start/end date timezones — this is the event's own zone hint.
    pub timezone: Option<String>,
    /// Number of file attachments on the event (bound-less today — just the count).
    pub attachments_count: usize,
    /// Attendees
    pub attendees: Vec<ParticipantInfo>,
    /// Event organizer
    pub organizer: Option<ParticipantInfo>,
}

/// Input for `EventsManager::create_event`. Only `title`, `start`, and `end`
/// are required; spread `..Default::default()` for the rest. Mirrors
/// `ReminderDraft` for consistency.
#[derive(Debug, Clone, Default)]
#[allow(non_snake_case)]
pub struct EventDraft<'a> {
    pub title: &'a str,
    pub start: Option<DateTime<Local>>,
    pub end: Option<DateTime<Local>>,
    pub notes: Option<&'a str>,
    pub location: Option<&'a str>,
    pub calendar_title: Option<&'a str>,
    pub all_day: bool,
    pub URL: Option<&'a str>,
    pub availability: Option<EventAvailability>,
    pub structured_location: Option<&'a StructuredLocation>,
}

/// Input for `EventsManager::update_event`. Each field uses one of:
/// `None` (don't touch), `Some(value)` (set). Nullable fields use
/// `Option<Option<T>>` so `Some(None)` clears the value. `span` controls
/// whether the edit applies to just this occurrence or all future ones in
/// a recurring series.
#[derive(Debug, Clone, Default)]
#[allow(non_snake_case)]
pub struct EventPatch<'a> {
    pub title: Option<&'a str>,
    pub notes: Option<Option<&'a str>>,
    pub location: Option<Option<&'a str>>,
    pub start: Option<DateTime<Local>>,
    pub end: Option<DateTime<Local>>,
    pub all_day: Option<bool>,
    /// Move to a different calendar by title.
    pub calendar_title: Option<&'a str>,
    pub URL: Option<Option<&'a str>>,
    pub availability: Option<EventAvailability>,
    pub structured_location: Option<Option<&'a StructuredLocation>>,
    /// `This` (default) edits only this occurrence; `Future` propagates.
    pub span: EventSpan,
}

/// The events manager providing access to Calendar events via EventKit
pub struct EventsManager {
    store: Retained<EKEventStore>,
}

impl EventsManager {
    /// Creates a new EventsManager instance.
    ///
    /// Cheap — see [`RemindersManager::new`] and the private `StoreCache`.
    pub fn new() -> Self {
        let store = EVENT_STORE.with(|cell| {
            cell.borrow_mut()
                .get_or_insert_with(StoreCache::build)
                .store
                .clone()
        });
        Self { store }
    }

    /// Gets the current authorization status for calendar events
    pub fn authorization_status() -> AuthorizationStatus {
        let status = unsafe { EKEventStore::authorizationStatusForEntityType(EKEntityType::Event) };
        status.into()
    }

    /// Requests full access to calendar events (blocking)
    ///
    /// Returns Ok(true) if access was granted, Ok(false) if denied
    pub fn request_access(&self) -> Result<bool> {
        let result = Arc::new((Mutex::new(None::<(bool, Option<String>)>), Condvar::new()));
        let result_clone = Arc::clone(&result);

        let completion = RcBlock::new(move |granted: Bool, error: *mut NSError| {
            let error_msg = if !error.is_null() {
                let error_ref = unsafe { &*error };
                Some(format!("{:?}", error_ref))
            } else {
                None
            };

            let (lock, cvar) = &*result_clone;
            let mut res = lock.lock();
            *res = Some((granted.as_bool(), error_msg));
            cvar.notify_one();
        });

        unsafe {
            let block_ptr = &*completion as *const _ as *mut _;
            self.store
                .requestFullAccessToEventsWithCompletion(block_ptr);
        }

        let (lock, cvar) = &*result;
        let mut res = lock.lock();
        if !wait_for_completion(cvar, &mut res, AUTHORIZATION_TIMEOUT) {
            return Err(EventKitError::AuthorizationRequestFailed(
                "timed out waiting for the system authorization response".to_string(),
            ));
        }

        match res.take() {
            Some((granted, None)) => Ok(granted),
            Some((_, Some(error))) => Err(EventKitError::AuthorizationRequestFailed(error)),
            None => Err(EventKitError::AuthorizationRequestFailed(
                "Unknown error".to_string(),
            )),
        }
    }

    /// Resolve an authorization check for `need`, prompting once if the user
    /// has not decided yet. See `RemindersManager::ensure_access`.
    fn ensure_access(&self, need: AccessNeed) -> Result<()> {
        let status = Self::authorization_status();
        Self::reconcile_store(status);
        match authorization_verdict(need, status) {
            AuthVerdict::Allowed => Ok(()),
            AuthVerdict::Refused(refusal) => Err(refusal.into_error()),
            AuthVerdict::MustRequest => {
                self.request_access()?;
                let status = Self::authorization_status();
                Self::reconcile_store(status);
                match authorization_verdict(need, status) {
                    AuthVerdict::Allowed => Ok(()),
                    AuthVerdict::Refused(refusal) => Err(refusal.into_error()),
                    AuthVerdict::MustRequest => Err(EventKitError::AuthorizationDenied),
                }
            }
        }
    }

    /// Reset this thread's cached store if the grant changed since last use.
    /// See `RemindersManager::reconcile_store` and [`StoreCache::reconcile`].
    fn reconcile_store(status: AuthorizationStatus) {
        EVENT_STORE.with(|cell| {
            if let Some(cache) = cell.borrow_mut().as_mut() {
                cache.reconcile(status);
            }
        });
    }

    /// Full access — required for ANY read/list/search/update/delete path.
    ///
    /// `WriteOnly` is an ERROR here, and for events this arm is LIVE (unlike
    /// reminders, events really do have a write-only tier). Apple:
    /// *"Your app can create events, but it can't access any of the existing
    /// calendars and events on the device, including events your app created.
    /// API calls to read event data from the event store don't return any
    /// events."* Allowing the call would report an empty calendar.
    pub fn ensure_full_access(&self) -> Result<()> {
        self.ensure_access(AccessNeed::Full)
    }

    /// Create-only paths. Both `FullAccess` and `WriteOnly` satisfy this.
    pub fn ensure_write_access(&self) -> Result<()> {
        self.ensure_access(AccessNeed::Write)
    }

    /// Lists all event calendars
    pub fn list_calendars(&self) -> Result<Vec<CalendarInfo>> {
        self.ensure_full_access()?;

        let calendars = unsafe { self.store.calendarsForEntityType(EKEntityType::Event) };

        let mut result = Vec::new();
        for calendar in calendars.iter() {
            result.push(calendar_to_info(&calendar));
        }

        Ok(result)
    }

    /// Gets the default calendar for new events
    pub fn default_calendar(&self) -> Result<CalendarInfo> {
        self.ensure_full_access()?;

        let calendar = unsafe { self.store.defaultCalendarForNewEvents() };

        match calendar {
            Some(cal) => Ok(calendar_to_info(&cal)),
            None => Err(EventKitError::NoDefaultCalendar),
        }
    }

    /// Fetches events for today
    pub fn fetch_today_events(&self) -> Result<Vec<EventItem>> {
        let now = Local::now();
        let start = now.date_naive().and_hms_opt(0, 0, 0).unwrap();
        let end = now.date_naive().and_hms_opt(23, 59, 59).unwrap();

        self.fetch_events(
            Local.from_local_datetime(&start).unwrap(),
            Local.from_local_datetime(&end).unwrap(),
            None,
        )
    }

    /// Fetches events for the next N days
    pub fn fetch_upcoming_events(&self, days: i64) -> Result<Vec<EventItem>> {
        let now = Local::now();
        let end = now + Duration::days(days);
        self.fetch_events(now, end, None)
    }

    /// Fetches events in a date range
    pub fn fetch_events(
        &self,
        start: DateTime<Local>,
        end: DateTime<Local>,
        calendar_titles: Option<&[&str]>,
    ) -> Result<Vec<EventItem>> {
        self.ensure_full_access()?;

        if start >= end {
            return Err(EventKitError::InvalidDateRange);
        }

        let calendars: Option<Retained<NSArray<EKCalendar>>> = match calendar_titles {
            Some(titles) => {
                let all_calendars =
                    unsafe { self.store.calendarsForEntityType(EKEntityType::Event) };
                let mut matching: Vec<Retained<EKCalendar>> = Vec::new();

                for cal in all_calendars.iter() {
                    let title = unsafe { cal.title() };
                    let title_str = title.to_string();
                    if titles.iter().any(|t| *t == title_str) {
                        matching.push(cal.retain());
                    }
                }

                if matching.is_empty() {
                    return Err(EventKitError::CalendarNotFound(titles.join(", ")));
                }

                Some(NSArray::from_retained_slice(&matching))
            }
            None => None,
        };

        let start_date = datetime_to_nsdate(start);
        let end_date = datetime_to_nsdate(end);

        let predicate = unsafe {
            self.store
                .predicateForEventsWithStartDate_endDate_calendars(
                    &start_date,
                    &end_date,
                    calendars.as_deref(),
                )
        };

        let events = unsafe { self.store.eventsMatchingPredicate(&predicate) };

        let mut items = Vec::new();
        for event in events.iter() {
            items.push(event_to_item(&event));
        }

        // Sort by start date
        items.sort_by_key(|a| a.start_date);

        Ok(items)
    }

    /// Creates a new event. Build the input with `EventDraft` — only
    /// `title`, `start`, and `end` are required; spread `..Default::default()`
    /// for the rest.
    pub fn create_event(&self, draft: &EventDraft<'_>) -> Result<EventItem> {
        // See `RemindersManager::create_reminder` — write-only covers a
        // default-calendar create, but not a by-title lookup.
        if draft.calendar_title.is_some() {
            self.ensure_full_access()?;
        } else {
            self.ensure_write_access()?;
        }

        let start = draft.start.ok_or(EventKitError::InvalidDateRange)?;
        let end = draft.end.ok_or(EventKitError::InvalidDateRange)?;

        let event = unsafe { EKEvent::eventWithEventStore(&self.store) };

        let ns_title = NSString::from_str(draft.title);
        unsafe { event.setTitle(Some(&ns_title)) };

        let start_date = datetime_to_nsdate(start);
        let end_date = datetime_to_nsdate(end);
        unsafe {
            event.setStartDate(Some(&start_date));
            event.setEndDate(Some(&end_date));
            event.setAllDay(draft.all_day);
        }

        if let Some(notes_text) = draft.notes {
            let ns_notes = NSString::from_str(notes_text);
            unsafe { event.setNotes(Some(&ns_notes)) };
        }

        if let Some(loc) = draft.location {
            let ns_location = NSString::from_str(loc);
            unsafe { event.setLocation(Some(&ns_location)) };
        }

        if draft.URL.is_some() {
            set_item_URL(&event, draft.URL)?;
        }

        if let Some(av) = draft.availability {
            unsafe { event.setAvailability(av.to_ek()) };
        }

        if let Some(sl) = draft.structured_location {
            let built = build_ek_structured_location(sl);
            unsafe { event.setStructuredLocation(Some(&built)) };
        }

        let calendar = if let Some(cal_title) = draft.calendar_title {
            self.find_calendar_by_title(cal_title)?
        } else {
            unsafe { self.store.defaultCalendarForNewEvents() }
                .ok_or(EventKitError::NoDefaultCalendar)?
        };
        unsafe { event.setCalendar(Some(&calendar)) };

        self.save_event_and_refresh(&event, EKSpan::ThisEvent)?;

        Ok(event_to_item(&event))
    }

    /// Updates an existing event
    /// Updates an existing event. Build the changeset with `EventPatch` —
    /// only the fields you set are written. For nullable fields,
    /// `Some(Some(v))` writes, `Some(None)` clears. `span` controls
    /// whether the edit applies to just this occurrence or all future
    /// occurrences in a recurring series.
    pub fn update_event(&self, identifier: &str, patch: &EventPatch<'_>) -> Result<EventItem> {
        self.ensure_full_access()?;

        let event = self.find_event_by_id(identifier)?;

        if let Some(t) = patch.title {
            let ns_title = NSString::from_str(t);
            unsafe { event.setTitle(Some(&ns_title)) };
        }

        if let Some(notes_opt) = patch.notes {
            let ns = notes_opt.map(NSString::from_str);
            unsafe { event.setNotes(ns.as_deref()) };
        }

        if let Some(loc_opt) = patch.location {
            let ns = loc_opt.map(NSString::from_str);
            unsafe { event.setLocation(ns.as_deref()) };
        }

        if let Some(s) = patch.start {
            let start_date = datetime_to_nsdate(s);
            unsafe { event.setStartDate(Some(&start_date)) };
        }

        if let Some(e) = patch.end {
            let end_date = datetime_to_nsdate(e);
            unsafe { event.setEndDate(Some(&end_date)) };
        }

        if let Some(ad) = patch.all_day {
            unsafe { event.setAllDay(ad) };
        }

        if let Some(url_opt) = patch.URL {
            set_item_URL(&event, url_opt)?;
        }

        if let Some(av) = patch.availability {
            unsafe { event.setAvailability(av.to_ek()) };
        }

        if let Some(sl_opt) = patch.structured_location {
            let built = sl_opt.map(build_ek_structured_location);
            unsafe { event.setStructuredLocation(built.as_deref()) };
        }

        if let Some(cal_title) = patch.calendar_title {
            let calendar = self.find_calendar_by_title(cal_title)?;
            unsafe { event.setCalendar(Some(&calendar)) };
        }

        self.save_event_and_refresh(&event, patch.span.to_ek())?;

        Ok(event_to_item(&event))
    }

    /// Deletes an event
    pub fn delete_event(&self, identifier: &str, affect_future: bool) -> Result<()> {
        self.ensure_full_access()?;

        let event = self.find_event_by_id(identifier)?;
        let span = if affect_future {
            EKSpan::FutureEvents
        } else {
            EKSpan::ThisEvent
        };

        unsafe {
            self.store
                .removeEvent_span_commit_error(&event, span, true)
                .map_err(|e| EventKitError::DeleteFailed(format!("{:?}", e)))?;
        }

        Ok(())
    }

    /// Gets an event by its identifier
    pub fn get_event(&self, identifier: &str) -> Result<EventItem> {
        self.ensure_full_access()?;
        let event = self.find_event_by_id(identifier)?;
        Ok(event_to_item(&event))
    }

    /// Probe an EVENT for undocumented Objective-C selectors, reporting the
    /// CLASS of every value returned.
    ///
    /// The reminders counterpart of this is
    /// [`RemindersManager::dump_reminder_private`]; both share one probe list
    /// so a finding on either class surfaces on both.
    ///
    /// Primary use today: settling whether a PRIVATE rich-notes property
    /// exists. The public API has none — `EKCalendarItem`'s entire "Attaching
    /// Notes" surface is `notes: NSString?` plus `hasNotes: Bool` — so the
    /// probe includes `notes` itself as a BASELINE. Compare the class it
    /// reports (expect `__NSCFString` / `NSTaggedPointerString`) against any
    /// `attributedNotes`-style hit; only a value whose class is
    /// `NSAttributedString`/`NSConcreteAttributedString`, or `NSData` carrying
    /// RTF, would mean rich text is reachable at all.
    pub fn dump_event_private(&self, identifier: &str) -> Result<String> {
        self.ensure_full_access()?;
        let event = self.find_event_by_id(identifier)?;
        let obj = unsafe { &*(&*event as *const EKEvent).cast::<objc2::runtime::AnyObject>() };
        Ok(probe_private_selectors(obj, "EKEvent"))
    }

    // ========================================================================
    // Event Calendar Management
    // ========================================================================

    /// Creates a new event calendar.
    pub fn create_event_calendar(&self, title: &str) -> Result<CalendarInfo> {
        self.ensure_full_access()?;
        let calendar = unsafe {
            EKCalendar::calendarForEntityType_eventStore(EKEntityType::Event, &self.store)
        };
        let ns_title = NSString::from_str(title);
        unsafe { calendar.setTitle(&ns_title) };

        // Use the default source
        if let Some(default_cal) = unsafe { self.store.defaultCalendarForNewEvents() }
            && let Some(source) = unsafe { default_cal.source() }
        {
            unsafe { calendar.setSource(Some(&source)) };
        }

        unsafe {
            self.store
                .saveCalendar_commit_error(&calendar, true)
                .map_err(|e| EventKitError::SaveFailed(format!("{:?}", e)))?;
        }
        Ok(calendar_to_info(&calendar))
    }

    /// Renames an event calendar.
    /// Rename an event calendar (backward compat wrapper).
    pub fn rename_event_calendar(&self, identifier: &str, new_title: &str) -> Result<CalendarInfo> {
        self.update_event_calendar(identifier, Some(new_title), None)
    }

    /// Update an event calendar — name, color, or both.
    pub fn update_event_calendar(
        &self,
        identifier: &str,
        new_title: Option<&str>,
        color_rgba: Option<(f64, f64, f64, f64)>,
    ) -> Result<CalendarInfo> {
        self.ensure_full_access()?;
        let calendar = unsafe {
            self.store
                .calendarWithIdentifier(&NSString::from_str(identifier))
        }
        .ok_or_else(|| EventKitError::CalendarNotFound(identifier.to_string()))?;

        if let Some(title) = new_title {
            let ns_title = NSString::from_str(title);
            unsafe { calendar.setTitle(&ns_title) };
        }

        if let Some((r, g, b, a)) = color_rgba {
            let cg = objc2_core_graphics::CGColor::new_srgb(r, g, b, a);
            unsafe { calendar.setCGColor(Some(&cg)) };
        }

        unsafe {
            self.store
                .saveCalendar_commit_error(&calendar, true)
                .map_err(|e| EventKitError::SaveFailed(format!("{:?}", e)))?;
        }
        Ok(calendar_to_info(&calendar))
    }

    /// Deletes an event calendar.
    pub fn delete_event_calendar(&self, identifier: &str) -> Result<()> {
        self.ensure_full_access()?;
        let calendar = unsafe {
            self.store
                .calendarWithIdentifier(&NSString::from_str(identifier))
        }
        .ok_or_else(|| EventKitError::CalendarNotFound(identifier.to_string()))?;

        unsafe {
            self.store
                .removeCalendar_commit_error(&calendar, true)
                .map_err(|e| EventKitError::DeleteFailed(format!("{:?}", e)))?;
        }
        Ok(())
    }

    // ========================================================================
    // Event Alarm Management (shared via EKCalendarItem)
    // ========================================================================

    /// Lists all alarms on an event.
    pub fn get_event_alarms(&self, identifier: &str) -> Result<Vec<AlarmInfo>> {
        self.ensure_full_access()?;
        let event = self.find_event_by_id(identifier)?;
        Ok(get_item_alarms(&event))
    }

    /// Adds an alarm to an event.
    pub fn add_event_alarm(&self, identifier: &str, alarm: &AlarmInfo) -> Result<()> {
        self.ensure_full_access()?;
        let event = self.find_event_by_id(identifier)?;
        add_item_alarm(&event, alarm)?;
        self.save_event_and_refresh(&event, EKSpan::ThisEvent)?;
        Ok(())
    }

    // ========================================================================
    // Event Recurrence Management (shared via EKCalendarItem)
    // ========================================================================

    /// Gets recurrence rules on an event.
    pub fn get_event_recurrence_rules(&self, identifier: &str) -> Result<Vec<RecurrenceRule>> {
        self.ensure_full_access()?;
        let event = self.find_event_by_id(identifier)?;
        Ok(get_item_recurrence_rules(&event))
    }

    /// Sets a recurrence rule on an event (replaces any existing rules).
    pub fn set_event_recurrence_rule(&self, identifier: &str, rule: &RecurrenceRule) -> Result<()> {
        self.ensure_full_access()?;
        let event = self.find_event_by_id(identifier)?;
        set_item_recurrence_rule(&event, rule);
        self.save_event_and_refresh(&event, EKSpan::ThisEvent)?;
        Ok(())
    }

    /// Removes all recurrence rules from an event.
    pub fn remove_event_recurrence_rules(&self, identifier: &str) -> Result<()> {
        self.ensure_full_access()?;
        let event = self.find_event_by_id(identifier)?;
        clear_item_recurrence_rules(&event);
        self.save_event_and_refresh(&event, EKSpan::ThisEvent)?;
        Ok(())
    }

    /// Removes a specific alarm from an event by index.
    pub fn remove_event_alarm(&self, identifier: &str, index: usize) -> Result<()> {
        self.ensure_full_access()?;
        let event = self.find_event_by_id(identifier)?;
        remove_item_alarm(&event, index)?;
        self.save_event_and_refresh(&event, EKSpan::ThisEvent)?;
        Ok(())
    }

    /// Set or clear the URL on an event.
    #[allow(non_snake_case)]
    pub fn set_event_URL(&self, identifier: &str, url: Option<&str>) -> Result<()> {
        self.ensure_full_access()?;
        let event = self.find_event_by_id(identifier)?;
        set_item_URL(&event, url)?;
        self.save_event_and_refresh(&event, EKSpan::ThisEvent)?;
        Ok(())
    }

    /// Set the event's `availability` field. Controls how the event shows
    /// on the timeline (Busy/Free/Tentative/Unavailable). Always uses
    /// `EKSpan::ThisEvent` since availability is typically per-instance.
    pub fn set_event_availability(
        &self,
        identifier: &str,
        availability: EventAvailability,
    ) -> Result<()> {
        self.ensure_full_access()?;
        let event = self.find_event_by_id(identifier)?;
        unsafe { event.setAvailability(availability.to_ek()) };
        self.save_event_and_refresh(&event, EKSpan::ThisEvent)
    }

    // Helper to find a calendar by title
    fn find_calendar_by_title(&self, title: &str) -> Result<Retained<EKCalendar>> {
        let calendars = unsafe { self.store.calendarsForEntityType(EKEntityType::Event) };

        for cal in calendars.iter() {
            let cal_title = unsafe { cal.title() };
            if cal_title.to_string() == title {
                return Ok(cal.retain());
            }
        }

        Err(EventKitError::CalendarNotFound(title.to_string()))
    }

    // Helper to find an event by identifier. Refreshes sources first so we
    // pick up edits made elsewhere (mirrors `RemindersManager::find_reminder_by_id`).
    fn find_event_by_id(&self, identifier: &str) -> Result<Retained<EKEvent>> {
        unsafe { self.store.refreshSourcesIfNecessary() };
        let ns_id = NSString::from_str(identifier);
        let event = unsafe { self.store.eventWithIdentifier(&ns_id) };

        match event {
            Some(e) => Ok(e),
            None => Err(EventKitError::ItemNotFound(identifier.to_string())),
        }
    }

    /// Save an event under the given span + refresh sources so subsequent
    /// reads see the just-committed state. All `pub fn` event save paths
    /// route through here, mirroring `RemindersManager::save_reminder_and_refresh`.
    fn save_event_and_refresh(&self, event: &EKEvent, span: EKSpan) -> Result<()> {
        unsafe {
            self.store
                .saveEvent_span_commit_error(event, span, true)
                .map_err(|e| EventKitError::SaveFailed(format!("{:?}", e)))?;
            self.store.refreshSourcesIfNecessary();
        }
        Ok(())
    }
}

impl Default for EventsManager {
    fn default() -> Self {
        Self::new()
    }
}

// Helper function to convert EKEvent to EventItem
fn event_to_item(event: &EKEvent) -> EventItem {
    let identifier = unsafe { event.eventIdentifier() }
        .map(|s| s.to_string())
        .unwrap_or_default();
    let title = unsafe { event.title() }.to_string();
    let notes = unsafe { event.notes() }.map(|n| n.to_string());
    let location = unsafe { event.location() }.map(|l| l.to_string());
    let all_day = unsafe { event.isAllDay() };
    let cal = unsafe { event.calendar() };
    let calendar_title = cal.as_ref().map(|c| unsafe { c.title() }.to_string());
    let calendar_id = cal
        .as_ref()
        .map(|c| unsafe { c.calendarIdentifier() }.to_string());

    let start_ns: Retained<NSDate> = unsafe { event.startDate() };
    let end_ns: Retained<NSDate> = unsafe { event.endDate() };
    let start_date = nsdate_to_datetime(&start_ns);
    let end_date = nsdate_to_datetime(&end_ns);

    #[allow(non_snake_case)]
    let URL = get_item_URL(event);

    let availability = EventAvailability::from_ek(unsafe { event.availability() });
    let status = EventStatus::from_ek(unsafe { event.status() });

    let is_detached = unsafe { event.isDetached() };
    let occurrence_date = unsafe { event.occurrenceDate() }.map(|d| nsdate_to_datetime(&d));

    // Structured location
    let structured_location = unsafe { event.structuredLocation() }.map(|loc| {
        let title = unsafe { loc.title() }
            .map(|t| t.to_string())
            .unwrap_or_default();
        let radius = unsafe { loc.radius() };
        let (latitude, longitude) = unsafe { loc.geoLocation() }
            .map(|geo| {
                let coord = unsafe { geo.coordinate() };
                (coord.latitude, coord.longitude)
            })
            .unwrap_or((0.0, 0.0));
        StructuredLocation {
            title,
            latitude,
            longitude,
            radius,
        }
    });

    // Attendees (shared via EKCalendarItem)
    let attendees = get_item_attendees(event);

    // Organizer (event-only)
    let organizer = unsafe { event.organizer() }.map(|p| participant_to_info(&p));

    // EKCalendarItem-inherited read fields (parity with ReminderItem)
    let creation_date = unsafe { event.creationDate() }.map(|d| nsdate_to_datetime(&d));
    let last_modified_date = unsafe { event.lastModifiedDate() }.map(|d| nsdate_to_datetime(&d));
    let external_identifier =
        unsafe { event.calendarItemExternalIdentifier() }.map(|s| s.to_string());
    let timezone = unsafe { event.timeZone() }.map(|tz| tz.name().to_string());
    let attachments_count = get_item_attachments_count(event);

    EventItem {
        identifier,
        title,
        notes,
        location,
        start_date,
        end_date,
        all_day,
        calendar_title,
        calendar_id,
        URL,
        availability,
        status,
        is_detached,
        occurrence_date,
        structured_location,
        creation_date,
        last_modified_date,
        external_identifier,
        timezone,
        attachments_count,
        attendees,
        organizer,
    }
}

// Read attendees from an EKCalendarItem (shared by events and reminders)
fn get_item_attendees(item: &EKCalendarItem) -> Vec<ParticipantInfo> {
    let attendees = unsafe { item.attendees() };
    let Some(attendees) = attendees else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for i in 0..attendees.len() {
        let p = attendees.objectAtIndex(i);
        result.push(participant_to_info(&p));
    }
    result
}

// Convert an EKParticipant to ParticipantInfo
fn participant_to_info(p: &objc2_event_kit::EKParticipant) -> ParticipantInfo {
    let name = unsafe { p.name() }.map(|n| n.to_string());
    #[allow(non_snake_case)]
    let URL = unsafe { p.URL() }.absoluteString().map(|s| s.to_string());

    // Role: 0=Unknown, 1=Required, 2=Optional, 3=Chair, 4=NonParticipant
    let role = unsafe { p.participantRole() };
    let role = match role.0 {
        1 => ParticipantRole::Required,
        2 => ParticipantRole::Optional,
        3 => ParticipantRole::Chair,
        4 => ParticipantRole::NonParticipant,
        _ => ParticipantRole::Unknown,
    };

    // Status: 0=Unknown, 1=Pending, 2=Accepted, 3=Declined, 4=Tentative,
    //         5=Delegated, 6=Completed, 7=InProcess
    let status = unsafe { p.participantStatus() };
    let status = match status.0 {
        1 => ParticipantStatus::Pending,
        2 => ParticipantStatus::Accepted,
        3 => ParticipantStatus::Declined,
        4 => ParticipantStatus::Tentative,
        5 => ParticipantStatus::Delegated,
        6 => ParticipantStatus::Completed,
        7 => ParticipantStatus::InProcess,
        _ => ParticipantStatus::Unknown,
    };

    let is_current_user = unsafe { p.isCurrentUser() };

    ParticipantInfo {
        name,
        URL,
        role,
        status,
        is_current_user,
    }
}

// Helper to convert chrono DateTime to NSDate
fn datetime_to_nsdate(dt: DateTime<Local>) -> Retained<NSDate> {
    let timestamp = dt.timestamp() as f64;
    NSDate::dateWithTimeIntervalSince1970(timestamp)
}

// Helper to convert NSDate to chrono DateTime
fn nsdate_to_datetime(date: &NSDate) -> DateTime<Local> {
    let timestamp = date.timeIntervalSince1970();
    Local.timestamp_opt(timestamp as i64, 0).unwrap()
}

// Helper to convert NSDateComponents to chrono DateTime
fn date_components_to_datetime(components: &NSDateComponents) -> Option<DateTime<Local>> {
    // Get a calendar to convert components to a date
    let calendar = NSCalendar::currentCalendar();

    // Convert components to NSDate using the calendar
    let date = calendar.dateFromComponents(components)?;

    Some(nsdate_to_datetime(&date))
}

// Helper to convert chrono DateTime to NSDateComponents
fn datetime_to_date_components(dt: DateTime<Local>) -> Retained<NSDateComponents> {
    let components = NSDateComponents::new();

    components.setYear(dt.year() as isize);
    components.setMonth(dt.month() as isize);
    components.setDay(dt.day() as isize);
    components.setHour(dt.hour() as isize);
    components.setMinute(dt.minute() as isize);
    components.setSecond(dt.second() as isize);

    components
}

// ============================================================================
// Shared EKCalendarItem operations
// ============================================================================
// EKCalendarItem is the base class for both EKReminder and EKEvent.
// These functions operate on the shared interface — both types auto-deref to it.

/// Read all alarms from a calendar item.
fn get_item_alarms(item: &EKCalendarItem) -> Vec<AlarmInfo> {
    let alarms = unsafe { item.alarms() };
    let Some(alarms) = alarms else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for i in 0..alarms.len() {
        let alarm = alarms.objectAtIndex(i);
        result.push(alarm_to_info(&alarm));
    }
    result
}

/// Add an alarm to a calendar item.
fn add_item_alarm(item: &EKCalendarItem, alarm: &AlarmInfo) -> Result<()> {
    let ek_alarm = create_ek_alarm(alarm)?;
    unsafe { item.addAlarm(&ek_alarm) };
    Ok(())
}

/// Remove an alarm from a calendar item by index.
fn remove_item_alarm(item: &EKCalendarItem, index: usize) -> Result<()> {
    let alarms = unsafe { item.alarms() };
    let Some(alarms) = alarms else {
        return Err(EventKitError::ItemNotFound("No alarms on this item".into()));
    };
    if index >= alarms.len() {
        return Err(EventKitError::ItemNotFound(format!(
            "Alarm index {} out of range ({})",
            index,
            alarms.len()
        )));
    }
    let alarm = alarms.objectAtIndex(index);
    unsafe { item.removeAlarm(&alarm) };
    Ok(())
}

/// Clear all alarms from a calendar item.
fn clear_item_alarms(item: &EKCalendarItem) {
    unsafe { item.setAlarms(None) };
}

/// Read all recurrence rules from a calendar item.
fn get_item_recurrence_rules(item: &EKCalendarItem) -> Vec<RecurrenceRule> {
    let rules = unsafe { item.recurrenceRules() };
    let Some(rules) = rules else {
        return Vec::new();
    };
    let mut result = Vec::new();
    for i in 0..rules.len() {
        let rule = rules.objectAtIndex(i);
        result.push(recurrence_rule_to_info(&rule));
    }
    result
}

/// Set a single recurrence rule on a calendar item (replaces any existing).
fn set_item_recurrence_rule(item: &EKCalendarItem, rule: &RecurrenceRule) {
    let ek_rule = create_ek_recurrence_rule(rule);
    unsafe {
        let rules = NSArray::from_retained_slice(&[ek_rule]);
        item.setRecurrenceRules(Some(&rules));
    }
}

/// Clear all recurrence rules from a calendar item.
fn clear_item_recurrence_rules(item: &EKCalendarItem) {
    unsafe { item.setRecurrenceRules(None) };
}

/// Set URL on a calendar item.
///
/// Uses `+[NSURL URLWithString:encodingInvalidCharacters:NO]` for strict
/// RFC 3986 validation — returns `Err(InvalidURL)` if the string isn't a
/// well-formed URL instead of silently auto-encoding or panicking.
#[allow(non_snake_case)]
fn set_item_URL(item: &EKCalendarItem, url: Option<&str>) -> Result<()> {
    let ns_url = match url {
        None => None,
        Some(s) => {
            let ns_str = NSString::from_str(s);
            let parsed =
                objc2_foundation::NSURL::URLWithString_encodingInvalidCharacters(&ns_str, false);
            match parsed {
                Some(u) => Some(u),
                None => return Err(EventKitError::InvalidURL(s.to_string())),
            }
        }
    };
    unsafe { item.setURL(ns_url.as_deref()) };
    Ok(())
}

/// Read URL from a calendar item.
#[allow(non_snake_case)]
fn get_item_URL(item: &EKCalendarItem) -> Option<String> {
    unsafe { item.URL() }.map(|u| u.absoluteString().unwrap().to_string())
}

/// Set the free-text `location` on a calendar item. Pass `None` to clear.
///
/// iCloud caveat: on iCloud-synced reminder lists the string `location`
/// property may silently revert to null even though `setLocation:` returns
/// success and the save commits. iCloud Reminders treats the location as a
/// derived view of `structuredLocation` set via a location-based alarm.
/// To attach a location that survives iCloud sync, use
/// `RemindersManager::set_geofence` (or pass `geofence` to a `ReminderDraft`).
fn set_item_location(item: &EKCalendarItem, location: Option<&str>) {
    match location {
        Some(text) => {
            let ns_loc = NSString::from_str(text);
            unsafe { item.setLocation(Some(&ns_loc)) };
        }
        None => unsafe { item.setLocation(None) },
    }
}

/// Set the timezone applied to the reminder's due date. EventKit derives
/// `EKReminder.dueDateTimeZone` from `dueDateComponents.timeZone` (the
/// `dueDateTimeZone` property is readonly), so the setter mutates the
/// components and re-assigns them. Pass `None` to clear.
///
/// No-op if the reminder has no due date yet — without `dueDateComponents`
/// there's nothing to hang the timezone on. Callers that want a timezone-
/// scoped reminder should set the due date first.
fn set_reminder_due_date_timezone(reminder: &EKReminder, tz_name: Option<&str>) {
    use objc2_foundation::NSTimeZone;
    let Some(components) = (unsafe { reminder.dueDateComponents() }) else {
        return;
    };
    let tz = tz_name.and_then(|n| {
        let ns_n = NSString::from_str(n);
        NSTimeZone::timeZoneWithName(&ns_n)
    });
    components.setTimeZone(tz.as_deref());
    unsafe { reminder.setDueDateComponents(Some(&components)) };
}

/// Set or clear the reminder's own `structuredLocation` — the iCloud-native
/// location chip Reminders.app reads/writes. Pass `None` to clear.
///
/// Uses `msg_send!` because `objc2-event-kit` 0.3 didn't generate
/// `setStructuredLocation:` on `EKCalendarItem` / `EKReminder` (only on
/// `EKEvent` and `EKAlarm`). The method exists at runtime — confirmed via
/// `class_copyMethodList`.
fn set_reminder_structured_location(reminder: &EKReminder, loc: Option<&StructuredLocation>) {
    use objc2::msg_send;
    let ek = loc.map(build_ek_structured_location);
    // EKCalendarItem exposes both — try the no-prediction variant first
    // (skips iCloud's "smart suggest based on title" pass), then the plain
    // setter. On iCloud-synced reminder lists the underlying daemon
    // silently drops both mutations; the alarm-based path (`set_geofence`)
    // is the only persistence route iCloud honors. We still call these
    // setters because local/CalDAV reminder sources accept them.
    let _: () =
        unsafe { msg_send![reminder, setStructuredLocationWithoutPrediction: ek.as_deref()] };
    let _: () = unsafe { msg_send![reminder, setStructuredLocation: ek.as_deref()] };
}

/// Build an `EKStructuredLocation` from our plain `StructuredLocation`.
/// Geo coordinates only set when the `location` feature is on (CoreLocation).
fn build_ek_structured_location(loc: &StructuredLocation) -> Retained<EKStructuredLocation> {
    let title = NSString::from_str(&loc.title);
    let structured = unsafe { EKStructuredLocation::locationWithTitle(&title) };
    unsafe { structured.setRadius(loc.radius) };
    #[cfg(feature = "location")]
    {
        use objc2::AnyThread;
        use objc2_core_location::CLLocation;
        let cl = unsafe {
            CLLocation::initWithLatitude_longitude(CLLocation::alloc(), loc.latitude, loc.longitude)
        };
        unsafe { structured.setGeoLocation(Some(&cl)) };
    }
    structured
}

/// Attach (or replace) a location-based alarm on the reminder. EventKit has
/// no `structuredLocation` directly on `EKCalendarItem`/`EKReminder` — the
/// geofence lives on an `EKAlarm` with `proximity`. Passing `None` clears
/// every proximity-bearing alarm.
fn set_reminder_geofence(
    reminder: &EKReminder,
    geofence: Option<(&StructuredLocation, AlarmProximity)>,
) {
    // Strip existing location-based alarms — leave time-based alarms alone.
    if let Some(alarms) = unsafe { reminder.alarms() } {
        for i in (0..alarms.len()).rev() {
            let alarm = alarms.objectAtIndex(i);
            if unsafe { alarm.proximity() } != EKAlarmProximity::None {
                unsafe { reminder.removeAlarm(&alarm) };
            }
        }
    }

    let Some((loc, proximity)) = geofence else {
        return;
    };

    let info = AlarmInfo {
        relative_offset: Some(0.0),
        proximity,
        location: Some(loc.clone()),
        ..Default::default()
    };
    // safe to expect — info has no `url`, the only field that can fail to parse
    let alarm = create_ek_alarm(&info).expect("geofence alarm has no URL to parse");
    unsafe { reminder.addAlarm(&alarm) };
}

// ============================================================================
// Type conversion helpers
// ============================================================================

// Helper to convert an EKRecurrenceRule to a RecurrenceRule
fn recurrence_rule_to_info(rule: &EKRecurrenceRule) -> RecurrenceRule {
    let frequency = unsafe { rule.frequency() };
    let frequency = match frequency {
        EKRecurrenceFrequency::Daily => RecurrenceFrequency::Daily,
        EKRecurrenceFrequency::Weekly => RecurrenceFrequency::Weekly,
        EKRecurrenceFrequency::Monthly => RecurrenceFrequency::Monthly,
        EKRecurrenceFrequency::Yearly => RecurrenceFrequency::Yearly,
        _ => RecurrenceFrequency::Daily,
    };

    let interval = unsafe { rule.interval() } as usize;

    let end = unsafe { rule.recurrenceEnd() }
        .map(|end| {
            let count = unsafe { end.occurrenceCount() };
            if count > 0 {
                RecurrenceEndCondition::AfterCount(count)
            } else if let Some(date) = unsafe { end.endDate() } {
                RecurrenceEndCondition::OnDate(nsdate_to_datetime(&date))
            } else {
                RecurrenceEndCondition::Never
            }
        })
        .unwrap_or(RecurrenceEndCondition::Never);

    let days_of_week = unsafe { rule.daysOfTheWeek() }.map(|days| {
        let mut result = Vec::new();
        for i in 0..days.len() {
            let day = days.objectAtIndex(i);
            let weekday = unsafe { day.dayOfTheWeek() };
            result.push(weekday.0 as u8);
        }
        result
    });

    fn nsnumber_array_to_vec(arr: Option<Retained<NSArray<NSNumber>>>) -> Option<Vec<i32>> {
        arr.map(|days| {
            let mut result = Vec::with_capacity(days.len());
            for i in 0..days.len() {
                result.push(days.objectAtIndex(i).intValue());
            }
            result
        })
    }

    let days_of_month = nsnumber_array_to_vec(unsafe { rule.daysOfTheMonth() });
    let months_of_year = nsnumber_array_to_vec(unsafe { rule.monthsOfTheYear() });
    let weeks_of_year = nsnumber_array_to_vec(unsafe { rule.weeksOfTheYear() });
    let days_of_year = nsnumber_array_to_vec(unsafe { rule.daysOfTheYear() });
    let set_positions = nsnumber_array_to_vec(unsafe { rule.setPositions() });

    RecurrenceRule {
        frequency,
        interval,
        end,
        days_of_week,
        days_of_month,
        months_of_year,
        weeks_of_year,
        days_of_year,
        set_positions,
    }
}

// Helper to create an EKRecurrenceRule from a RecurrenceRule
fn create_ek_recurrence_rule(rule: &RecurrenceRule) -> Retained<EKRecurrenceRule> {
    let frequency = match rule.frequency {
        RecurrenceFrequency::Daily => EKRecurrenceFrequency::Daily,
        RecurrenceFrequency::Weekly => EKRecurrenceFrequency::Weekly,
        RecurrenceFrequency::Monthly => EKRecurrenceFrequency::Monthly,
        RecurrenceFrequency::Yearly => EKRecurrenceFrequency::Yearly,
    };

    let end = match &rule.end {
        RecurrenceEndCondition::Never => None,
        RecurrenceEndCondition::AfterCount(count) => {
            Some(unsafe { EKRecurrenceEnd::recurrenceEndWithOccurrenceCount(*count) })
        }
        RecurrenceEndCondition::OnDate(date) => {
            let nsdate = datetime_to_nsdate(*date);
            Some(unsafe { EKRecurrenceEnd::recurrenceEndWithEndDate(&nsdate) })
        }
    };

    let days_of_week: Option<Vec<Retained<EKRecurrenceDayOfWeek>>> =
        rule.days_of_week.as_ref().map(|days| {
            days.iter()
                .map(|&d| {
                    let weekday = EKWeekday(d as isize);
                    unsafe { EKRecurrenceDayOfWeek::dayOfWeek(weekday) }
                })
                .collect()
        });

    fn vec_to_nsnumber_array(v: Option<&Vec<i32>>) -> Option<Vec<Retained<NSNumber>>> {
        v.map(|nums| nums.iter().map(|&n| NSNumber::new_i32(n)).collect())
    }
    let days_of_month = vec_to_nsnumber_array(rule.days_of_month.as_ref());
    let months_of_year = vec_to_nsnumber_array(rule.months_of_year.as_ref());
    let weeks_of_year = vec_to_nsnumber_array(rule.weeks_of_year.as_ref());
    let days_of_year = vec_to_nsnumber_array(rule.days_of_year.as_ref());
    let set_positions = vec_to_nsnumber_array(rule.set_positions.as_ref());

    let days_of_week_arr = days_of_week
        .as_ref()
        .map(|v| NSArray::from_retained_slice(v));
    let days_of_month_arr = days_of_month
        .as_ref()
        .map(|v| NSArray::from_retained_slice(v));
    let months_of_year_arr = months_of_year
        .as_ref()
        .map(|v| NSArray::from_retained_slice(v));
    let weeks_of_year_arr = weeks_of_year
        .as_ref()
        .map(|v| NSArray::from_retained_slice(v));
    let days_of_year_arr = days_of_year
        .as_ref()
        .map(|v| NSArray::from_retained_slice(v));
    let set_positions_arr = set_positions
        .as_ref()
        .map(|v| NSArray::from_retained_slice(v));

    unsafe {
        use objc2::AnyThread;
        EKRecurrenceRule::initRecurrenceWithFrequency_interval_daysOfTheWeek_daysOfTheMonth_monthsOfTheYear_weeksOfTheYear_daysOfTheYear_setPositions_end(
            EKRecurrenceRule::alloc(),
            frequency,
            rule.interval as isize,
            days_of_week_arr.as_deref(),
            days_of_month_arr.as_deref(),
            months_of_year_arr.as_deref(),
            weeks_of_year_arr.as_deref(),
            days_of_year_arr.as_deref(),
            set_positions_arr.as_deref(),
            end.as_deref(),
        )
    }
}

// Helper to convert an EKAlarm to an AlarmInfo
fn alarm_to_info(alarm: &EKAlarm) -> AlarmInfo {
    let relative_offset = unsafe { alarm.relativeOffset() };
    let absolute_date = unsafe { alarm.absoluteDate() }.map(|d| nsdate_to_datetime(&d));

    let proximity = unsafe { alarm.proximity() };
    let proximity = match proximity {
        EKAlarmProximity::Enter => AlarmProximity::Enter,
        EKAlarmProximity::Leave => AlarmProximity::Leave,
        _ => AlarmProximity::None,
    };

    let location = unsafe { alarm.structuredLocation() }.map(|loc| {
        let title = unsafe { loc.title() }
            .map(|t| t.to_string())
            .unwrap_or_default();
        let radius = unsafe { loc.radius() };
        let (latitude, longitude) = unsafe { loc.geoLocation() }
            .map(|geo| {
                let coord = unsafe { geo.coordinate() };
                (coord.latitude, coord.longitude)
            })
            .unwrap_or((0.0, 0.0));

        StructuredLocation {
            title,
            latitude,
            longitude,
            radius,
        }
    });

    let email_address = unsafe { alarm.emailAddress() }.map(|s| s.to_string());
    let sound_name = unsafe { alarm.soundName() }.map(|s| s.to_string());
    #[allow(deprecated)]
    let url = unsafe { alarm.url() }
        .as_ref()
        .and_then(|u| u.absoluteString())
        .map(|s| s.to_string());

    let alarm_type = match unsafe { alarm.r#type() } {
        EKAlarmType::Display => AlarmType::Display,
        EKAlarmType::Audio => AlarmType::Audio,
        EKAlarmType::Procedure => AlarmType::Procedure,
        EKAlarmType::Email => AlarmType::Email,
        _ => AlarmType::Unknown,
    };

    AlarmInfo {
        // relativeOffset of 0 means "at time of event" — it's always set
        relative_offset: Some(relative_offset),
        absolute_date,
        proximity,
        location,
        email_address,
        sound_name,
        url,
        alarm_type,
    }
}

// Helper to create an EKAlarm from an AlarmInfo. Returns Err if the
// caller-supplied `url` isn't a valid RFC 3986 URL.
fn create_ek_alarm(info: &AlarmInfo) -> Result<Retained<EKAlarm>> {
    let alarm = if let Some(date) = &info.absolute_date {
        let nsdate = datetime_to_nsdate(*date);
        unsafe { EKAlarm::alarmWithAbsoluteDate(&nsdate) }
    } else {
        let offset = info.relative_offset.unwrap_or(0.0);
        unsafe { EKAlarm::alarmWithRelativeOffset(offset) }
    };

    let prox = match info.proximity {
        AlarmProximity::Enter => EKAlarmProximity::Enter,
        AlarmProximity::Leave => EKAlarmProximity::Leave,
        AlarmProximity::None => EKAlarmProximity::None,
    };
    unsafe { alarm.setProximity(prox) };

    if let Some(loc) = &info.location {
        let structured = build_ek_structured_location(loc);
        unsafe { alarm.setStructuredLocation(Some(&structured)) };
    }

    // Apple infers EKAlarmType from which of these are set:
    //   soundName → Audio, url → Procedure, emailAddress → Email, else Display.
    if let Some(email) = &info.email_address {
        let ns = NSString::from_str(email);
        unsafe { alarm.setEmailAddress(Some(&ns)) };
    }
    if let Some(sound) = &info.sound_name {
        let ns = NSString::from_str(sound);
        unsafe { alarm.setSoundName(Some(&ns)) };
    }
    if let Some(u) = &info.url {
        let ns_str = NSString::from_str(u);
        let parsed =
            objc2_foundation::NSURL::URLWithString_encodingInvalidCharacters(&ns_str, false)
                .ok_or_else(|| EventKitError::InvalidURL(u.clone()))?;
        #[allow(deprecated)]
        unsafe {
            alarm.setUrl(Some(&parsed))
        };
    }

    Ok(alarm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_span_default_is_this_and_maps_to_ek() {
        assert_eq!(EventSpan::default(), EventSpan::This);
        assert_eq!(EventSpan::This.to_ek(), EKSpan::ThisEvent);
        assert_eq!(EventSpan::Future.to_ek(), EKSpan::FutureEvents);
    }

    #[test]
    fn event_availability_round_trips_every_variant() {
        for v in [
            EventAvailability::NotSupported,
            EventAvailability::Busy,
            EventAvailability::Free,
            EventAvailability::Tentative,
            EventAvailability::Unavailable,
        ] {
            assert_eq!(EventAvailability::from_ek(v.to_ek()), v);
        }
    }

    #[test]
    fn event_status_from_ek_covers_each_variant() {
        assert_eq!(EventStatus::from_ek(EKEventStatus::None), EventStatus::None);
        assert_eq!(
            EventStatus::from_ek(EKEventStatus::Confirmed),
            EventStatus::Confirmed
        );
        assert_eq!(
            EventStatus::from_ek(EKEventStatus::Tentative),
            EventStatus::Tentative
        );
        assert_eq!(
            EventStatus::from_ek(EKEventStatus::Canceled),
            EventStatus::Canceled
        );
    }

    #[test]
    fn event_draft_default_has_empty_title_and_zero_dates() {
        // Documents that defaults aren't valid for actually creating —
        // callers must populate title/start/end. Just confirms the struct
        // builds with `..Default::default()` so the call sites can use it.
        let d: EventDraft<'_> = EventDraft::default();
        assert_eq!(d.title, "");
        assert!(d.start.is_none());
        assert!(d.end.is_none());
        assert!(!d.all_day);
        assert!(d.URL.is_none());
        assert!(d.availability.is_none());
    }

    #[test]
    fn event_patch_default_touches_nothing() {
        let p: EventPatch<'_> = EventPatch::default();
        assert!(p.title.is_none());
        assert!(p.notes.is_none());
        assert!(p.URL.is_none());
        assert!(p.availability.is_none());
        assert!(p.all_day.is_none());
        assert_eq!(p.span, EventSpan::This);
    }

    #[test]
    fn test_authorization_status_display() {
        assert_eq!(
            format!("{}", AuthorizationStatus::NotDetermined),
            "Not Determined"
        );
        assert_eq!(
            format!("{}", AuthorizationStatus::FullAccess),
            "Full Access"
        );
    }

    // ── Authorization decision seam ──────────────────────────────────────
    //
    // `authorization_verdict` takes the status as a PARAMETER precisely so
    // these run identically on a granted dev machine and on CI. Every one of
    // the five statuses is covered for both access needs.

    /// THE regression guard. A write-only grant must REFUSE a read, because
    /// Apple returns no events at all to a write-only client — so `Allowed`
    /// here produces `Ok(vec![])` and the model tells the user their calendar
    /// is empty when it simply lacks permission. Assert on the refusal, since
    /// silent success is exactly the bug.
    #[test]
    fn write_only_refuses_reads_never_silently_allows() {
        assert_eq!(
            authorization_verdict(AccessNeed::Full, AuthorizationStatus::WriteOnly),
            AuthVerdict::Refused(AuthRefusal::WriteOnly),
            "write-only must ERROR on a read path, not return an empty list"
        );
    }

    /// ...but a write-only grant still satisfies a create-only path.
    #[test]
    fn write_only_allows_writes() {
        assert_eq!(
            authorization_verdict(AccessNeed::Write, AuthorizationStatus::WriteOnly),
            AuthVerdict::Allowed
        );
    }

    #[test]
    fn full_access_allows_both_needs() {
        assert_eq!(
            authorization_verdict(AccessNeed::Full, AuthorizationStatus::FullAccess),
            AuthVerdict::Allowed
        );
        assert_eq!(
            authorization_verdict(AccessNeed::Write, AuthorizationStatus::FullAccess),
            AuthVerdict::Allowed
        );
    }

    #[test]
    fn not_determined_requests_for_both_needs() {
        for need in [AccessNeed::Full, AccessNeed::Write] {
            assert_eq!(
                authorization_verdict(need, AuthorizationStatus::NotDetermined),
                AuthVerdict::MustRequest,
                "{need:?} must prompt, not decide"
            );
        }
    }

    #[test]
    fn denied_and_restricted_refuse_for_both_needs() {
        for need in [AccessNeed::Full, AccessNeed::Write] {
            assert_eq!(
                authorization_verdict(need, AuthorizationStatus::Denied),
                AuthVerdict::Refused(AuthRefusal::Denied),
                "{need:?} + Denied"
            );
            assert_eq!(
                authorization_verdict(need, AuthorizationStatus::Restricted),
                AuthVerdict::Refused(AuthRefusal::Restricted),
                "{need:?} + Restricted"
            );
        }
    }

    /// The three refusals must stay DISTINCT errors — a write-only user needs
    /// "grant full access", not "you denied us".
    #[test]
    fn refusals_map_to_distinct_errors() {
        assert!(matches!(
            AuthRefusal::WriteOnly.into_error(),
            EventKitError::AuthorizationWriteOnly
        ));
        assert!(matches!(
            AuthRefusal::Denied.into_error(),
            EventKitError::AuthorizationDenied
        ));
        assert!(matches!(
            AuthRefusal::Restricted.into_error(),
            EventKitError::AuthorizationRestricted
        ));
    }

    // ── Store cache / reset-on-transition ────────────────────────────────
    //
    // `StoreCache::reconcile` is tested through a decision helper rather than
    // a real `EKEventStore`, for the same reason as the verdict table: a real
    // store needs live TCC state. What matters is WHEN we reset, and that is
    // pure logic over (previous, current).

    /// THE coupling guard. Caching the store without resetting it on a grant
    /// transition reintroduces silently-empty results by a different route: a
    /// user who grants access mid-session keeps getting nothing until restart.
    /// This is the test that fails if someone removes the `reset()`.
    #[test]
    fn grant_transition_resets_the_cached_store() {
        assert!(
            needs_store_reset(
                Some(AuthorizationStatus::NotDetermined),
                AuthorizationStatus::FullAccess
            ),
            "NotDetermined → FullAccess MUST reset, or reads stay empty after the grant"
        );
        assert!(
            needs_store_reset(
                Some(AuthorizationStatus::WriteOnly),
                AuthorizationStatus::FullAccess
            ),
            "WriteOnly → FullAccess MUST reset"
        );
        assert!(
            needs_store_reset(
                Some(AuthorizationStatus::Denied),
                AuthorizationStatus::FullAccess
            ),
            "Denied → FullAccess MUST reset"
        );
    }

    /// `reset()` discards uncommitted changes, so it must NOT fire when the
    /// grant is unchanged — that would silently drop a caller's in-flight edits.
    #[test]
    fn unchanged_status_does_not_reset() {
        for status in [
            AuthorizationStatus::FullAccess,
            AuthorizationStatus::WriteOnly,
            AuthorizationStatus::Denied,
            AuthorizationStatus::Restricted,
            AuthorizationStatus::NotDetermined,
        ] {
            assert!(
                !needs_store_reset(Some(status), status),
                "{status:?} unchanged must NOT reset (it discards uncommitted changes)"
            );
        }
    }

    /// First use on a thread has read nothing yet, so there is no stale state
    /// to clear — resetting a brand-new store is pure overhead.
    #[test]
    fn first_use_does_not_reset() {
        for status in [
            AuthorizationStatus::FullAccess,
            AuthorizationStatus::NotDetermined,
        ] {
            assert!(!needs_store_reset(None, status), "first use of {status:?}");
        }
    }

    /// Constructing a manager must not touch ANY EventKit accessor.
    ///
    /// Regression guard. `objc2` declares most `EKEventStore` accessors
    /// non-null, but they return NULL to an unauthorized process, which aborts
    /// the thread. An `eventStoreIdentifier()` call in `StoreCache::build`
    /// panicked before `ensure_full_access` could run, so `request_access()`
    /// never fired and no TCC consent dialog ever appeared.
    ///
    /// This test can only prove construction is panic-free on THIS host (which
    /// may be authorized); the real protection is the rule documented on
    /// `StoreCache::build`. Keep construction inert.
    #[test]
    fn constructing_a_manager_touches_no_eventkit_accessor() {
        let _ = RemindersManager::new();
        let _ = EventsManager::new();
        // Reaching here means neither constructor aborted.
    }

    /// A manager construction must REUSE the thread's store, not build a new
    /// one — that is the whole point of the cache. Asserting on pointer
    /// identity rather than timing keeps it deterministic.
    #[test]
    fn managers_reuse_one_store_per_thread() {
        let a = RemindersManager::new();
        let b = RemindersManager::new();
        assert!(
            std::ptr::eq(&*a.store, &*b.store),
            "repeated RemindersManager::new() must retain ONE cached EKEventStore"
        );

        let c = EventsManager::new();
        let d = EventsManager::new();
        assert!(
            std::ptr::eq(&*c.store, &*d.store),
            "repeated EventsManager::new() must retain ONE cached EKEventStore"
        );
    }

    // ── Completion-block timeouts ────────────────────────────────────────
    //
    // `wait_for_completion` is the guard against a permanent worker-thread
    // wedge: every EventKit call in this file parks a thread until an Obj-C
    // block fires, and a block that never fires used to park it forever. The
    // helper takes a plain `Condvar`, so the timeout is testable with no
    // EventKit and no TCC state — which matters, because the failure it exists
    // to prevent only reproduces on a host that can't answer a consent dialog.

    /// A completion that NEVER fires must give up, not park forever.
    #[test]
    fn wait_for_completion_gives_up_when_nothing_arrives() {
        let lock = Mutex::new(None::<u8>);
        let cvar = Condvar::new();
        let mut guard = lock.lock();

        let start = std::time::Instant::now();
        let arrived = wait_for_completion(&cvar, &mut guard, std::time::Duration::from_millis(150));
        let waited = start.elapsed();

        assert!(!arrived, "must report timeout, not a phantom completion");
        assert!(guard.is_none(), "the slot must be left untouched");
        assert!(
            waited >= std::time::Duration::from_millis(150),
            "must actually wait the full budget, waited {waited:?}"
        );
        assert!(
            waited < std::time::Duration::from_secs(5),
            "must not park past the budget, waited {waited:?}"
        );
    }

    /// The happy path still works: a block that fires wakes the waiter and its
    /// value is delivered. Without this, a timeout that always returned false
    /// would pass the test above.
    #[test]
    fn wait_for_completion_returns_the_value_a_block_publishes() {
        let pair = Arc::new((Mutex::new(None::<u8>), Condvar::new()));
        let writer = Arc::clone(&pair);

        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let (lock, cvar) = &*writer;
            *lock.lock() = Some(7);
            cvar.notify_one();
        });

        let (lock, cvar) = &*pair;
        let mut guard = lock.lock();
        let arrived = wait_for_completion(cvar, &mut guard, std::time::Duration::from_secs(5));

        assert!(arrived, "a completion that fires must be observed");
        assert_eq!(guard.take(), Some(7));
    }

    /// A value already in the slot must return immediately — the loop condition
    /// is checked before waiting, so this must not depend on a notification
    /// that already came and went.
    #[test]
    fn wait_for_completion_does_not_wait_on_an_already_full_slot() {
        let lock = Mutex::new(Some(1u8));
        let cvar = Condvar::new();
        let mut guard = lock.lock();

        let start = std::time::Instant::now();
        // A long budget: if this consulted the condvar it would hang here.
        let arrived = wait_for_completion(&cvar, &mut guard, std::time::Duration::from_secs(30));

        assert!(arrived);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(1),
            "must return immediately when the slot is already full"
        );
    }

    /// EXACTLY ONE place in this file may construct a store.
    ///
    /// This guards the outage that shipped before the cache existed: both
    /// manager constructors built a store unconditionally, and `mcp.rs` builds a
    /// manager per tool call, so a busy session allocated one store per call
    /// until the calendar daemon cut the process off with
    /// `EKCADErrorDomain 1021 — "This process has too many EKEventStore
    /// instances. Use fewer event stores."`
    ///
    /// The fix routes every construction through `StoreCache::build`, behind the
    /// per-thread caches. A second construction site anywhere in this file would
    /// quietly restore the old behaviour, and no behavioural test would notice:
    /// exhaustion needs enough LIVE stores to trip a process-wide daemon limit,
    /// which no unit test creates and CI would never reach. The source itself is
    /// the only thing that can be asserted on cheaply.
    ///
    /// The needle is split so this test cannot match itself.
    #[test]
    fn only_one_place_constructs_a_store() {
        let needle = concat!("EKEventStore", "::new()");
        let hits = include_str!("imp.rs").matches(needle).count();
        assert_eq!(
            hits, 1,
            "expected exactly ONE construction site (StoreCache::build), found {hits}. \
             Every store must come from the per-thread cache — see StoreCache."
        );
    }

    /// The write-only error text must name FULL access — the app surfaces this
    /// string, and "denied" would send the user to the wrong remedy.
    #[test]
    fn write_only_error_names_full_access() {
        let msg = EventKitError::AuthorizationWriteOnly.to_string();
        assert!(
            msg.to_lowercase().contains("full access"),
            "must tell the user to grant FULL access, got: {msg}"
        );
        assert!(
            msg.contains("System Settings"),
            "must name where to fix it, got: {msg}"
        );
    }

    #[test]
    fn test_event_item_debug() {
        let event = EventItem {
            identifier: "test".to_string(),
            title: "Test Event".to_string(),
            notes: None,
            location: None,
            start_date: Local::now(),
            end_date: Local::now(),
            all_day: false,
            calendar_title: None,
            calendar_id: None,
            URL: None,
            availability: EventAvailability::Busy,
            status: EventStatus::None,
            is_detached: false,
            occurrence_date: None,
            structured_location: None,
            creation_date: None,
            last_modified_date: None,
            external_identifier: None,
            timezone: None,
            attachments_count: 0,
            attendees: Vec::new(),
            organizer: None,
        };
        assert!(format!("{:?}", event).contains("Test Event"));
    }
}
