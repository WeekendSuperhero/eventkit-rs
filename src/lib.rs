//! # EventKit-RS
//!
//! A Rust library for interacting with macOS Calendar and Reminders via EventKit.
//!
//! This library provides safe wrappers around the Apple EventKit framework to:
//! - Request and check authorization for calendar and reminders access
//! - List, create, update, and delete calendar events
//! - List, create, update, and delete reminders
//! - Manage calendars and reminder lists
//!
//! ## Quick Start
//!
//! ```rust,no_run
//! use eventkit::{RemindersManager, EventsManager, Result};
//!
//! fn main() -> Result<()> {
//!     // Working with reminders
//!     let reminders = RemindersManager::new();
//!     reminders.request_access()?;
//!
//!     for reminder in reminders.fetch_incomplete_reminders()? {
//!         println!("Todo: {}", reminder.title);
//!     }
//!
//!     // Working with calendar events
//!     let events = EventsManager::new();
//!     events.request_access()?;
//!
//!     for event in events.fetch_today_events()? {
//!         println!("Event: {}", event.title);
//!     }
//!
//!     Ok(())
//! }
//! ```
//!
//! ## Platform Support
//!
//! This library only works on macOS. It requires macOS 10.14 or later for full functionality.
//!
//! ## Privacy Permissions
//!
//! Your application will need to request calendar and/or reminders permissions.
//! Make sure to include the appropriate keys in your `Info.plist`:
//!
//! - `NSRemindersUsageDescription` - for reminders access
//! - `NSCalendarsFullAccessUsageDescription` - for calendar access (macOS 14+)
//! - `NSCalendarsUsageDescription` - for calendar access (older macOS)

use block2::RcBlock;
use chrono::{DateTime, Duration, Local, TimeZone};
use objc2::Message;
use objc2::rc::Retained;
use objc2::runtime::Bool;
use objc2_event_kit::{
    EKAuthorizationStatus, EKCalendar, EKEntityType, EKEvent, EKEventStore, EKReminder, EKSpan,
};
use objc2_foundation::{NSArray, NSDate, NSError, NSString};
use std::sync::{Arc, Condvar, Mutex};
use thiserror::Error;

/// Errors that can occur when working with EventKit
#[derive(Error, Debug)]
pub enum EventKitError {
    #[error("Authorization denied")]
    AuthorizationDenied,

    #[error("Authorization restricted by system policy")]
    AuthorizationRestricted,

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
}

/// Backward compatibility alias
pub type RemindersError = EventKitError;

/// Result type for EventKit operations
pub type Result<T> = std::result::Result<T, EventKitError>;

/// Represents a reminder item with its properties
#[derive(Debug, Clone)]
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
}

/// Represents a calendar (reminder list)
#[derive(Debug, Clone)]
pub struct CalendarInfo {
    /// Unique identifier
    pub identifier: String,
    /// Title of the calendar
    pub title: String,
    /// Source name (e.g., iCloud, Local)
    pub source: Option<String>,
    /// Whether content can be modified
    pub allows_modifications: bool,
}

/// The main reminders manager providing access to EventKit functionality
pub struct RemindersManager {
    store: Retained<EKEventStore>,
}

impl RemindersManager {
    /// Creates a new RemindersManager instance
    pub fn new() -> Self {
        let store = unsafe { EKEventStore::new() };
        Self { store }
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
            let mut res = lock.lock().unwrap();
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
        let mut res = lock.lock().unwrap();
        while res.is_none() {
            res = cvar.wait(res).unwrap();
        }

        match res.take() {
            Some((granted, None)) => Ok(granted),
            Some((_, Some(error))) => Err(RemindersError::AuthorizationRequestFailed(error)),
            None => Err(RemindersError::AuthorizationRequestFailed(
                "Unknown error".to_string(),
            )),
        }
    }

    /// Ensures we have authorization, requesting if needed
    pub fn ensure_authorized(&self) -> Result<()> {
        match Self::authorization_status() {
            AuthorizationStatus::FullAccess => Ok(()),
            AuthorizationStatus::NotDetermined => {
                if self.request_access()? {
                    Ok(())
                } else {
                    Err(RemindersError::AuthorizationDenied)
                }
            }
            AuthorizationStatus::Denied => Err(RemindersError::AuthorizationDenied),
            AuthorizationStatus::Restricted => Err(RemindersError::AuthorizationRestricted),
            AuthorizationStatus::WriteOnly => Ok(()), // Can still read with write-only in some cases
        }
    }

    /// Lists all reminder calendars (lists)
    pub fn list_calendars(&self) -> Result<Vec<CalendarInfo>> {
        self.ensure_authorized()?;

        let calendars = unsafe { self.store.calendarsForEntityType(EKEntityType::Reminder) };

        let mut result = Vec::new();
        for calendar in calendars.iter() {
            result.push(calendar_to_info(&calendar));
        }

        Ok(result)
    }

    /// Gets the default calendar for new reminders
    pub fn default_calendar(&self) -> Result<CalendarInfo> {
        self.ensure_authorized()?;

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
        self.ensure_authorized()?;

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
            let mut guard = lock.lock().unwrap();
            *guard = Some(items);
            cvar.notify_one();
        });

        unsafe {
            self.store
                .fetchRemindersMatchingPredicate_completion(&predicate, &completion);
        }

        let (lock, cvar) = &*result;
        let mut guard = lock.lock().unwrap();
        while guard.is_none() {
            guard = cvar.wait(guard).unwrap();
        }

        guard
            .take()
            .ok_or_else(|| RemindersError::FetchFailed("Unknown error".to_string()))
    }

    /// Fetches incomplete reminders
    pub fn fetch_incomplete_reminders(&self) -> Result<Vec<ReminderItem>> {
        self.ensure_authorized()?;

        let predicate = unsafe {
            self.store
                .predicateForIncompleteRemindersWithDueDateStarting_ending_calendars(
                    None, None, None,
                )
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
            let mut guard = lock.lock().unwrap();
            *guard = Some(items);
            cvar.notify_one();
        });

        unsafe {
            self.store
                .fetchRemindersMatchingPredicate_completion(&predicate, &completion);
        }

        let (lock, cvar) = &*result;
        let mut guard = lock.lock().unwrap();
        while guard.is_none() {
            guard = cvar.wait(guard).unwrap();
        }

        guard
            .take()
            .ok_or_else(|| RemindersError::FetchFailed("Unknown error".to_string()))
    }

    /// Creates a new reminder
    pub fn create_reminder(
        &self,
        title: &str,
        notes: Option<&str>,
        calendar_title: Option<&str>,
        priority: Option<usize>,
    ) -> Result<ReminderItem> {
        self.ensure_authorized()?;

        let reminder = unsafe { EKReminder::reminderWithEventStore(&self.store) };

        // Set title
        let ns_title = NSString::from_str(title);
        unsafe { reminder.setTitle(Some(&ns_title)) };

        // Set notes if provided
        if let Some(notes_text) = notes {
            let ns_notes = NSString::from_str(notes_text);
            unsafe { reminder.setNotes(Some(&ns_notes)) };
        }

        // Set priority if provided
        if let Some(p) = priority {
            unsafe { reminder.setPriority(p) };
        }

        // Set calendar
        let calendar = if let Some(cal_title) = calendar_title {
            self.find_calendar_by_title(cal_title)?
        } else {
            unsafe { self.store.defaultCalendarForNewReminders() }
                .ok_or(RemindersError::NoDefaultCalendar)?
        };
        unsafe { reminder.setCalendar(Some(&calendar)) };

        // Save
        unsafe {
            self.store
                .saveReminder_commit_error(&reminder, true)
                .map_err(|e| RemindersError::SaveFailed(format!("{:?}", e)))?;
        }

        Ok(reminder_to_item(&reminder))
    }

    /// Updates an existing reminder
    pub fn update_reminder(
        &self,
        identifier: &str,
        title: Option<&str>,
        notes: Option<&str>,
        completed: Option<bool>,
        priority: Option<usize>,
    ) -> Result<ReminderItem> {
        self.ensure_authorized()?;

        let reminder = self.find_reminder_by_id(identifier)?;

        if let Some(t) = title {
            let ns_title = NSString::from_str(t);
            unsafe { reminder.setTitle(Some(&ns_title)) };
        }

        if let Some(n) = notes {
            let ns_notes = NSString::from_str(n);
            unsafe { reminder.setNotes(Some(&ns_notes)) };
        }

        if let Some(c) = completed {
            unsafe { reminder.setCompleted(c) };
        }

        if let Some(p) = priority {
            unsafe { reminder.setPriority(p) };
        }

        unsafe {
            self.store
                .saveReminder_commit_error(&reminder, true)
                .map_err(|e| RemindersError::SaveFailed(format!("{:?}", e)))?;
        }

        Ok(reminder_to_item(&reminder))
    }

    /// Marks a reminder as complete
    pub fn complete_reminder(&self, identifier: &str) -> Result<ReminderItem> {
        self.update_reminder(identifier, None, None, Some(true), None)
    }

    /// Marks a reminder as incomplete
    pub fn uncomplete_reminder(&self, identifier: &str) -> Result<ReminderItem> {
        self.update_reminder(identifier, None, None, Some(false), None)
    }

    /// Deletes a reminder
    pub fn delete_reminder(&self, identifier: &str) -> Result<()> {
        self.ensure_authorized()?;

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
        self.ensure_authorized()?;
        let reminder = self.find_reminder_by_id(identifier)?;
        Ok(reminder_to_item(&reminder))
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
    fn find_reminder_by_id(&self, identifier: &str) -> Result<Retained<EKReminder>> {
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

// Helper function to convert EKReminder to ReminderItem
fn reminder_to_item(reminder: &EKReminder) -> ReminderItem {
    let identifier = unsafe { reminder.calendarItemIdentifier() }.to_string();
    let title = unsafe { reminder.title() }.to_string();
    let notes = unsafe { reminder.notes() }.map(|n| n.to_string());
    let completed = unsafe { reminder.isCompleted() };
    let priority = unsafe { reminder.priority() };
    let calendar_title = unsafe { reminder.calendar() }.map(|c| unsafe { c.title() }.to_string());

    ReminderItem {
        identifier,
        title,
        notes,
        completed,
        priority,
        calendar_title,
    }
}

// Helper function to convert EKCalendar to CalendarInfo
fn calendar_to_info(calendar: &EKCalendar) -> CalendarInfo {
    let identifier = unsafe { calendar.calendarIdentifier() }.to_string();
    let title = unsafe { calendar.title() }.to_string();
    let source = unsafe { calendar.source() }.map(|s| unsafe { s.title() }.to_string());
    let allows_modifications = unsafe { calendar.allowsContentModifications() };

    CalendarInfo {
        identifier,
        title,
        source,
        allows_modifications,
    }
}

// ============================================================================
// Calendar Events Support
// ============================================================================

/// Represents a calendar event with its properties
#[derive(Debug, Clone)]
pub struct EventItem {
    /// Unique identifier for the event
    pub identifier: String,
    /// Title of the event
    pub title: String,
    /// Optional notes/description
    pub notes: Option<String>,
    /// Optional location
    pub location: Option<String>,
    /// Start date/time
    pub start_date: DateTime<Local>,
    /// End date/time
    pub end_date: DateTime<Local>,
    /// Whether this is an all-day event
    pub all_day: bool,
    /// Calendar the event belongs to
    pub calendar_title: Option<String>,
}

/// The events manager providing access to Calendar events via EventKit
pub struct EventsManager {
    store: Retained<EKEventStore>,
}

impl EventsManager {
    /// Creates a new EventsManager instance
    pub fn new() -> Self {
        let store = unsafe { EKEventStore::new() };
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
            let mut res = lock.lock().unwrap();
            *res = Some((granted.as_bool(), error_msg));
            cvar.notify_one();
        });

        unsafe {
            let block_ptr = &*completion as *const _ as *mut _;
            self.store
                .requestFullAccessToEventsWithCompletion(block_ptr);
        }

        let (lock, cvar) = &*result;
        let mut res = lock.lock().unwrap();
        while res.is_none() {
            res = cvar.wait(res).unwrap();
        }

        match res.take() {
            Some((granted, None)) => Ok(granted),
            Some((_, Some(error))) => Err(EventKitError::AuthorizationRequestFailed(error)),
            None => Err(EventKitError::AuthorizationRequestFailed(
                "Unknown error".to_string(),
            )),
        }
    }

    /// Ensures we have authorization, requesting if needed
    pub fn ensure_authorized(&self) -> Result<()> {
        match Self::authorization_status() {
            AuthorizationStatus::FullAccess => Ok(()),
            AuthorizationStatus::NotDetermined => {
                if self.request_access()? {
                    Ok(())
                } else {
                    Err(EventKitError::AuthorizationDenied)
                }
            }
            AuthorizationStatus::Denied => Err(EventKitError::AuthorizationDenied),
            AuthorizationStatus::Restricted => Err(EventKitError::AuthorizationRestricted),
            AuthorizationStatus::WriteOnly => Ok(()),
        }
    }

    /// Lists all event calendars
    pub fn list_calendars(&self) -> Result<Vec<CalendarInfo>> {
        self.ensure_authorized()?;

        let calendars = unsafe { self.store.calendarsForEntityType(EKEntityType::Event) };

        let mut result = Vec::new();
        for calendar in calendars.iter() {
            result.push(calendar_to_info(&calendar));
        }

        Ok(result)
    }

    /// Gets the default calendar for new events
    pub fn default_calendar(&self) -> Result<CalendarInfo> {
        self.ensure_authorized()?;

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
        self.ensure_authorized()?;

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
        items.sort_by(|a, b| a.start_date.cmp(&b.start_date));

        Ok(items)
    }

    /// Creates a new event
    #[allow(clippy::too_many_arguments)]
    pub fn create_event(
        &self,
        title: &str,
        start: DateTime<Local>,
        end: DateTime<Local>,
        notes: Option<&str>,
        location: Option<&str>,
        calendar_title: Option<&str>,
        all_day: bool,
    ) -> Result<EventItem> {
        self.ensure_authorized()?;

        let event = unsafe { EKEvent::eventWithEventStore(&self.store) };

        // Set title
        let ns_title = NSString::from_str(title);
        unsafe { event.setTitle(Some(&ns_title)) };

        // Set dates
        let start_date = datetime_to_nsdate(start);
        let end_date = datetime_to_nsdate(end);
        unsafe {
            event.setStartDate(Some(&start_date));
            event.setEndDate(Some(&end_date));
            event.setAllDay(all_day);
        }

        // Set notes if provided
        if let Some(notes_text) = notes {
            let ns_notes = NSString::from_str(notes_text);
            unsafe { event.setNotes(Some(&ns_notes)) };
        }

        // Set location if provided
        if let Some(loc) = location {
            let ns_location = NSString::from_str(loc);
            unsafe { event.setLocation(Some(&ns_location)) };
        }

        // Set calendar
        let calendar = if let Some(cal_title) = calendar_title {
            self.find_calendar_by_title(cal_title)?
        } else {
            unsafe { self.store.defaultCalendarForNewEvents() }
                .ok_or(EventKitError::NoDefaultCalendar)?
        };
        unsafe { event.setCalendar(Some(&calendar)) };

        // Save
        unsafe {
            self.store
                .saveEvent_span_error(&event, EKSpan::ThisEvent)
                .map_err(|e| EventKitError::SaveFailed(format!("{:?}", e)))?;
        }

        Ok(event_to_item(&event))
    }

    /// Updates an existing event
    pub fn update_event(
        &self,
        identifier: &str,
        title: Option<&str>,
        notes: Option<&str>,
        location: Option<&str>,
        start: Option<DateTime<Local>>,
        end: Option<DateTime<Local>>,
    ) -> Result<EventItem> {
        self.ensure_authorized()?;

        let event = self.find_event_by_id(identifier)?;

        if let Some(t) = title {
            let ns_title = NSString::from_str(t);
            unsafe { event.setTitle(Some(&ns_title)) };
        }

        if let Some(n) = notes {
            let ns_notes = NSString::from_str(n);
            unsafe { event.setNotes(Some(&ns_notes)) };
        }

        if let Some(l) = location {
            let ns_location = NSString::from_str(l);
            unsafe { event.setLocation(Some(&ns_location)) };
        }

        if let Some(s) = start {
            let start_date = datetime_to_nsdate(s);
            unsafe { event.setStartDate(Some(&start_date)) };
        }

        if let Some(e) = end {
            let end_date = datetime_to_nsdate(e);
            unsafe { event.setEndDate(Some(&end_date)) };
        }

        unsafe {
            self.store
                .saveEvent_span_error(&event, EKSpan::ThisEvent)
                .map_err(|e| EventKitError::SaveFailed(format!("{:?}", e)))?;
        }

        Ok(event_to_item(&event))
    }

    /// Deletes an event
    pub fn delete_event(&self, identifier: &str) -> Result<()> {
        self.ensure_authorized()?;

        let event = self.find_event_by_id(identifier)?;

        unsafe {
            self.store
                .removeEvent_span_error(&event, EKSpan::ThisEvent)
                .map_err(|e| EventKitError::DeleteFailed(format!("{:?}", e)))?;
        }

        Ok(())
    }

    /// Gets an event by its identifier
    pub fn get_event(&self, identifier: &str) -> Result<EventItem> {
        self.ensure_authorized()?;
        let event = self.find_event_by_id(identifier)?;
        Ok(event_to_item(&event))
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

    // Helper to find an event by identifier
    fn find_event_by_id(&self, identifier: &str) -> Result<Retained<EKEvent>> {
        let ns_id = NSString::from_str(identifier);
        let event = unsafe { self.store.eventWithIdentifier(&ns_id) };

        match event {
            Some(e) => Ok(e),
            None => Err(EventKitError::ItemNotFound(identifier.to_string())),
        }
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
    let calendar_title = unsafe { event.calendar() }.map(|c| unsafe { c.title() }.to_string());

    let start_ns: Retained<NSDate> = unsafe { event.startDate() };
    let end_ns: Retained<NSDate> = unsafe { event.endDate() };

    let start_date = nsdate_to_datetime(&start_ns);
    let end_date = nsdate_to_datetime(&end_ns);

    EventItem {
        identifier,
        title,
        notes,
        location,
        start_date,
        end_date,
        all_day,
        calendar_title,
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

#[cfg(test)]
mod tests {
    use super::*;

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
        };
        assert!(format!("{:?}", event).contains("Test Event"));
    }
}
