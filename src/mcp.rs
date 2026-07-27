//! EventKit MCP Server
//!
//! A Model Context Protocol (MCP) server that exposes macOS Calendar and Reminders
//! functionality via the EventKit framework.
//!
//! This module is gated behind the `mcp` feature flag.

// `RoleServer` / `service::RequestContext` are NOT imported: since rmcp-macros
// 2.x (#866) the `#[prompt_handler]` / `#[tool_handler]` expansions name those
// types with fully qualified paths, so they no longer have to be in scope.
use rmcp::{
    ErrorData as McpError, ServiceExt, handler::server::wrapper::Parameters, model::*, prompt,
    prompt_handler, prompt_router, schemars, schemars::JsonSchema, tool, tool_handler, tool_router,
    transport::stdio,
};
use serde::{Deserialize, Serialize};

use crate::{AuthorizationStatus, EventsManager, RemindersManager};
use chrono::{DateTime, Duration, Local, NaiveDateTime, TimeZone};

use rmcp::handler::server::wrapper::Json;

// ============================================================================
// Structured Output Types
// ============================================================================

/// Convert an EventKitError into an McpError for tool returns.
///
/// Authorization variants get expanded with a remediation hint so the agent
/// can guide the user to fix the underlying TCC state.
fn mcp_err(e: &crate::EventKitError) -> McpError {
    use crate::EventKitError::*;
    let msg = match e {
        AuthorizationDenied => {
            "Reminders/Calendar access denied. Open System Settings → Privacy & Security \
             and enable access for `eventkit`. If `eventkit` is not listed, run \
             `tccutil reset Reminders` (or Calendar) in a terminal and retry. \
             Call `auth_status` to see the current state."
                .to_string()
        }
        AuthorizationRestricted => {
            "Reminders/Calendar access is restricted by system policy (MDM or parental controls)."
                .to_string()
        }
        AuthorizationWriteOnly => {
            // NAME FULL ACCESS explicitly. "Denied" would send the user to the
            // wrong remedy: they DID grant something, just not enough. Apple
            // returns no events at all to a write-only client, so without this
            // error the tool would have answered with an empty calendar.
            "Only WRITE-ONLY Calendar access was granted, which cannot read anything — \
             Apple returns no events to a write-only client, not even ones this app created. \
             Open System Settings → Privacy & Security → Calendars and grant FULL access \
             for `eventkit`. Call `auth_status` to see the current state."
                .to_string()
        }
        AuthorizationNotDetermined => {
            "Authorization is undetermined and the consent dialog did not fire. \
             The binary may be missing Info.plist usage strings — rebuild and retry. \
             Call `auth_status` to see the current state."
                .to_string()
        }
        AuthorizationRequestFailed(detail) => {
            format!("Authorization request failed: {detail}")
        }
        _ => e.to_string(),
    };
    McpError::internal_error(msg, None)
}

fn mcp_invalid(msg: impl std::fmt::Display) -> McpError {
    McpError::invalid_params(msg.to_string(), None)
}

#[derive(Serialize, JsonSchema)]
struct ListResponse<T: Serialize> {
    #[schemars(with = "i64")]
    count: usize,
    items: Vec<T>,
}

#[derive(Serialize, JsonSchema)]
struct DeletedResponse {
    id: String,
}

#[derive(Serialize, JsonSchema)]
struct BatchResponse {
    #[schemars(with = "i64")]
    total: usize,
    #[schemars(with = "i64")]
    succeeded: usize,
    #[schemars(with = "i64")]
    failed: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<BatchItemError>,
}

#[derive(Serialize, JsonSchema)]
struct BatchItemError {
    item_id: String,
    message: String,
}

#[derive(Serialize, JsonSchema)]
struct SearchResponse {
    query: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reminders: Option<ListResponse<ReminderOutput>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    events: Option<ListResponse<EventOutput>>,
}

#[derive(Serialize, JsonSchema)]
struct CoordinateOutput {
    latitude: f64,
    longitude: f64,
}

#[derive(Serialize, JsonSchema)]
struct AuthStatusOutput {
    /// One of: "FullAccess", "WriteOnly", "Denied", "NotDetermined", "Restricted"
    reminders: &'static str,
    /// One of: "FullAccess", "WriteOnly", "Denied", "NotDetermined", "Restricted"
    events: &'static str,
    /// Human-readable next step. Absent when both statuses are FullAccess/WriteOnly.
    #[serde(skip_serializing_if = "Option::is_none")]
    remediation: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum AccessEntity {
    Reminder,
    Event,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RequestAccessRequest {
    /// Which EventKit entity to request access for.
    pub entity: AccessEntity,
}

#[derive(Serialize, JsonSchema)]
struct RequestAccessOutput {
    granted: bool,
    /// Status after the request. One of: "FullAccess", "WriteOnly",
    /// "Denied", "NotDetermined", "Restricted".
    status: &'static str,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SetReminderDueTimezoneRequest {
    pub reminder_id: String,
    /// IANA zone name, e.g. `"America/Los_Angeles"`. Pass `null`/`""` to clear.
    pub timezone: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GeofenceProximity {
    Enter,
    Leave,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GeofenceInput {
    pub title: String,
    pub latitude: f64,
    pub longitude: f64,
    /// Radius in meters.
    pub radius_meters: f64,
    /// Trigger when entering vs leaving the radius.
    pub proximity: GeofenceProximity,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SetReminderGeofenceRequest {
    pub reminder_id: String,
    /// Pass the geofence to attach, or omit/null to clear any existing
    /// location-based alarm on the reminder.
    pub geofence: Option<GeofenceInput>,
}

/// Structured location metadata for an event — title + lat/lng + a display
/// radius. Unlike `GeofenceInput`, there's no proximity trigger; events use
/// this for travel-time, map preview, and Siri suggestions.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct StructuredLocationInput {
    pub title: String,
    pub latitude: f64,
    pub longitude: f64,
    /// Display radius in meters (0 = labeled point with no radius).
    pub radius_meters: f64,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SetEventAvailabilityRequest {
    pub event_id: String,
    /// "busy" | "free" | "tentative" | "unavailable".
    pub availability: String,
}

/// Parse an availability string into our `EventAvailability` enum.
/// Accepts: "busy", "free", "tentative", "unavailable", "not_supported".
fn parse_availability(s: &str) -> Result<crate::EventAvailability, String> {
    match s {
        "busy" => Ok(crate::EventAvailability::Busy),
        "free" => Ok(crate::EventAvailability::Free),
        "tentative" => Ok(crate::EventAvailability::Tentative),
        "unavailable" => Ok(crate::EventAvailability::Unavailable),
        "not_supported" => Ok(crate::EventAvailability::NotSupported),
        other => Err(format!(
            "invalid availability '{other}'. Use busy, free, tentative, unavailable, or not_supported."
        )),
    }
}

/// Parse a span string ("this" | "future") into our `EventSpan` enum.
/// Defaults to `This` if omitted.
fn parse_span(s: Option<&str>) -> Result<crate::EventSpan, String> {
    match s {
        None | Some("this") => Ok(crate::EventSpan::This),
        Some("future") => Ok(crate::EventSpan::Future),
        Some(other) => Err(format!(
            "invalid span '{other}'. Use \"this\" or \"future\"."
        )),
    }
}

fn auth_status_str(s: AuthorizationStatus) -> &'static str {
    match s {
        AuthorizationStatus::NotDetermined => "NotDetermined",
        AuthorizationStatus::Restricted => "Restricted",
        AuthorizationStatus::Denied => "Denied",
        AuthorizationStatus::FullAccess => "FullAccess",
        AuthorizationStatus::WriteOnly => "WriteOnly",
    }
}

fn auth_remediation(reminders: AuthorizationStatus, events: AuthorizationStatus) -> Option<String> {
    // ONLY FullAccess counts as granted. `WriteOnly` used to be treated as
    // granted here, which meant a write-only user was told everything was
    // fine while every read silently returned nothing — the same defect as
    // the old `WriteOnly => Ok(())` in `ensure_authorized`, surfacing at the
    // `auth_status` tool instead of the read path.
    let granted = |s| matches!(s, AuthorizationStatus::FullAccess);
    if granted(reminders) && granted(events) {
        return None;
    }
    let worst = |s| match s {
        AuthorizationStatus::Denied => 4,
        AuthorizationStatus::Restricted => 3,
        // Above NotDetermined: the user already engaged with the prompt, so
        // the actionable fix is a specific Settings change, not "try again".
        AuthorizationStatus::WriteOnly => 2,
        AuthorizationStatus::NotDetermined => 1,
        _ => 0,
    };
    let pick = if worst(reminders) >= worst(events) {
        reminders
    } else {
        events
    };
    Some(match pick {
        AuthorizationStatus::NotDetermined => {
            "Call any reminders or calendar tool to trigger the macOS consent dialog. \
             If no dialog appears, the binary is missing its Info.plist usage strings — \
             rebuild from a version that embeds them."
                .into()
        }
        AuthorizationStatus::Denied => {
            "Open System Settings → Privacy & Security → Reminders (and/or Calendar) and \
             enable access for `eventkit`. If `eventkit` is not listed, run \
             `tccutil reset Reminders` (or `tccutil reset Calendar`) in a terminal and \
             retry — that clears the cached denial so the consent dialog can fire again."
                .into()
        }
        AuthorizationStatus::Restricted => {
            "Access is blocked by an MDM or parental-controls policy. The user cannot \
             override this from System Settings — an administrator must change the policy."
                .into()
        }
        AuthorizationStatus::WriteOnly => {
            "Only WRITE-ONLY access was granted. Apple returns no events or reminders at all \
             to a write-only client, so reads would come back empty rather than fail. Open \
             System Settings → Privacy & Security → Calendars (and/or Reminders) and grant \
             FULL access for `eventkit`."
                .into()
        }
        _ => "Authorization is partially granted; one of reminders/events is not in FullAccess state.".into(),
    })
}

#[derive(Serialize, JsonSchema)]
struct LocationOutput {
    title: String,
    latitude: f64,
    longitude: f64,
    radius_meters: f64,
}

#[derive(Serialize, JsonSchema)]
struct AlarmOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    relative_offset_seconds: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    absolute_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    proximity: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<LocationOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    email_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sound_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    /// Derived from which optional fields are set: "display" | "audio" |
    /// "procedure" | "email" | "unknown".
    alarm_type: String,
}

impl AlarmOutput {
    fn from_info(a: &crate::AlarmInfo) -> Self {
        Self {
            relative_offset_seconds: a.relative_offset,
            absolute_date: a.absolute_date.map(|d| d.to_rfc3339()),
            proximity: match a.proximity {
                crate::AlarmProximity::Enter => Some("enter".into()),
                crate::AlarmProximity::Leave => Some("leave".into()),
                crate::AlarmProximity::None => None,
            },
            location: a.location.as_ref().map(|l| LocationOutput {
                title: l.title.clone(),
                latitude: l.latitude,
                longitude: l.longitude,
                radius_meters: l.radius,
            }),
            email_address: a.email_address.clone(),
            sound_name: a.sound_name.clone(),
            url: a.url.clone(),
            alarm_type: match a.alarm_type {
                crate::AlarmType::Display => "display",
                crate::AlarmType::Audio => "audio",
                crate::AlarmType::Procedure => "procedure",
                crate::AlarmType::Email => "email",
                crate::AlarmType::Unknown => "unknown",
            }
            .into(),
        }
    }
}

#[derive(Serialize, JsonSchema)]
struct RecurrenceRuleOutput {
    frequency: String,
    #[schemars(with = "i64")]
    interval: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(with = "Option<Vec<i32>>")]
    days_of_week: Option<Vec<u8>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    days_of_month: Option<Vec<i32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    months_of_year: Option<Vec<i32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    weeks_of_year: Option<Vec<i32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    days_of_year: Option<Vec<i32>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    set_positions: Option<Vec<i32>>,
    end: RecurrenceEndOutput,
}

#[derive(Serialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
enum RecurrenceEndOutput {
    Never,
    AfterCount {
        #[schemars(with = "i64")]
        count: usize,
    },
    OnDate {
        date: String,
    },
}

impl RecurrenceRuleOutput {
    fn from_rule(r: &crate::RecurrenceRule) -> Self {
        Self {
            frequency: match r.frequency {
                crate::RecurrenceFrequency::Daily => "daily",
                crate::RecurrenceFrequency::Weekly => "weekly",
                crate::RecurrenceFrequency::Monthly => "monthly",
                crate::RecurrenceFrequency::Yearly => "yearly",
            }
            .into(),
            interval: r.interval,
            days_of_week: r.days_of_week.clone(),
            days_of_month: r.days_of_month.clone(),
            months_of_year: r.months_of_year.clone(),
            weeks_of_year: r.weeks_of_year.clone(),
            days_of_year: r.days_of_year.clone(),
            set_positions: r.set_positions.clone(),
            end: match &r.end {
                crate::RecurrenceEndCondition::Never => RecurrenceEndOutput::Never,
                crate::RecurrenceEndCondition::AfterCount(n) => {
                    RecurrenceEndOutput::AfterCount { count: *n }
                }
                crate::RecurrenceEndCondition::OnDate(d) => RecurrenceEndOutput::OnDate {
                    date: d.to_rfc3339(),
                },
            },
        }
    }
}

#[derive(Serialize, JsonSchema)]
struct AttendeeOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    role: String,
    status: String,
    is_current_user: bool,
}

impl AttendeeOutput {
    fn from_info(p: &crate::ParticipantInfo) -> Self {
        Self {
            name: p.name.clone(),
            role: format!("{:?}", p.role).to_lowercase(),
            status: format!("{:?}", p.status).to_lowercase(),
            is_current_user: p.is_current_user,
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[allow(non_snake_case)]
struct ReminderOutput {
    id: String,
    title: String,
    completed: bool,
    priority: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    list_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    list_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    due_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    completion_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    URL: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<String>,
    /// IANA zone applied to the due date specifically (separate from the
    /// item-level timezone). Lets a reminder fire at the same wall-clock
    /// time regardless of the device's current zone.
    #[serde(skip_serializing_if = "Option::is_none")]
    due_date_timezone: Option<String>,
    /// Geofence attached via a location-based alarm.
    #[serde(skip_serializing_if = "Option::is_none")]
    geofence: Option<LocationOutput>,
    /// Parent reminder identifier when this is a subtask.
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_id: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    attachments_count: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    alarms: Vec<AlarmOutput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recurrence_rules: Vec<RecurrenceRuleOutput>,
}

fn is_zero(n: &usize) -> bool {
    *n == 0
}

impl ReminderOutput {
    fn from_item(r: &crate::ReminderItem, manager: &RemindersManager) -> Self {
        let alarms = if r.has_alarms {
            manager
                .get_alarms(&r.identifier)
                .unwrap_or_default()
                .iter()
                .map(AlarmOutput::from_info)
                .collect()
        } else {
            vec![]
        };
        let recurrence_rules = if r.has_recurrence_rules {
            manager
                .get_recurrence_rules(&r.identifier)
                .unwrap_or_default()
                .iter()
                .map(RecurrenceRuleOutput::from_rule)
                .collect()
        } else {
            vec![]
        };
        Self {
            alarms,
            recurrence_rules,
            ..Self::from_item_summary(r)
        }
    }

    fn from_item_summary(r: &crate::ReminderItem) -> Self {
        Self {
            id: r.identifier.clone(),
            title: r.title.clone(),
            completed: r.completed,
            priority: Priority::label(r.priority).into(),
            list_name: r.calendar_title.clone(),
            list_id: r.calendar_id.clone(),
            due_date: r.due_date.map(|d| d.to_rfc3339()),
            start_date: r.start_date.map(|d| d.to_rfc3339()),
            completion_date: r.completion_date.map(|d| d.to_rfc3339()),
            notes: r.notes.clone(),
            URL: r.URL.clone(),
            location: r.location.clone(),
            due_date_timezone: r.due_date_timezone.clone(),
            geofence: r.structured_location.as_ref().map(|s| LocationOutput {
                title: s.title.clone(),
                latitude: s.latitude,
                longitude: s.longitude,
                radius_meters: s.radius,
            }),
            parent_id: r.parent_id.clone(),
            attachments_count: r.attachments_count,
            alarms: vec![],
            recurrence_rules: vec![],
        }
    }
}

#[derive(Serialize, JsonSchema)]
#[allow(non_snake_case)]
struct EventOutput {
    id: String,
    title: String,
    start: String,
    end: String,
    all_day: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    calendar_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    calendar_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    location: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    URL: Option<String>,
    availability: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    structured_location: Option<LocationOutput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    alarms: Vec<AlarmOutput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recurrence_rules: Vec<RecurrenceRuleOutput>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    attendees: Vec<AttendeeOutput>,
    #[serde(skip_serializing_if = "Option::is_none")]
    organizer: Option<AttendeeOutput>,
    is_detached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    occurrence_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    creation_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_modified_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    external_identifier: Option<String>,
    /// Item-level timezone hint (`EKCalendarItem.timeZone`), distinct from
    /// the timezone applied to `start` / `end`.
    #[serde(skip_serializing_if = "Option::is_none")]
    timezone: Option<String>,
    #[serde(skip_serializing_if = "is_zero")]
    attachments_count: usize,
}

impl EventOutput {
    fn from_item(e: &crate::EventItem, manager: &EventsManager) -> Self {
        let alarms = manager
            .get_event_alarms(&e.identifier)
            .unwrap_or_default()
            .iter()
            .map(AlarmOutput::from_info)
            .collect();
        let recurrence_rules = manager
            .get_event_recurrence_rules(&e.identifier)
            .unwrap_or_default()
            .iter()
            .map(RecurrenceRuleOutput::from_rule)
            .collect();
        Self {
            alarms,
            recurrence_rules,
            ..Self::from_item_summary(e)
        }
    }

    fn from_item_summary(e: &crate::EventItem) -> Self {
        Self {
            id: e.identifier.clone(),
            title: e.title.clone(),
            start: e.start_date.to_rfc3339(),
            end: e.end_date.to_rfc3339(),
            all_day: e.all_day,
            calendar_name: e.calendar_title.clone(),
            calendar_id: e.calendar_id.clone(),
            notes: e.notes.clone(),
            location: e.location.clone(),
            URL: e.URL.clone(),
            availability: match e.availability {
                crate::EventAvailability::Busy => "busy",
                crate::EventAvailability::Free => "free",
                crate::EventAvailability::Tentative => "tentative",
                crate::EventAvailability::Unavailable => "unavailable",
                _ => "not_supported",
            }
            .into(),
            status: match e.status {
                crate::EventStatus::Confirmed => "confirmed",
                crate::EventStatus::Tentative => "tentative",
                crate::EventStatus::Canceled => "canceled",
                _ => "none",
            }
            .into(),
            structured_location: e.structured_location.as_ref().map(|l| LocationOutput {
                title: l.title.clone(),
                latitude: l.latitude,
                longitude: l.longitude,
                radius_meters: l.radius,
            }),
            alarms: vec![],
            recurrence_rules: vec![],
            attendees: e.attendees.iter().map(AttendeeOutput::from_info).collect(),
            organizer: e.organizer.as_ref().map(AttendeeOutput::from_info),
            is_detached: e.is_detached,
            occurrence_date: e.occurrence_date.map(|d| d.to_rfc3339()),
            creation_date: e.creation_date.map(|d| d.to_rfc3339()),
            last_modified_date: e.last_modified_date.map(|d| d.to_rfc3339()),
            external_identifier: e.external_identifier.clone(),
            timezone: e.timezone.clone(),
            attachments_count: e.attachments_count,
        }
    }
}

#[derive(Serialize, JsonSchema)]
struct CalendarOutput {
    id: String,
    title: String,
    color: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_id: Option<String>,
    allows_modifications: bool,
    is_immutable: bool,
    is_subscribed: bool,
    entity_types: Vec<String>,
    /// Which `availability` values this calendar accepts on its events:
    /// subset of `["busy", "free", "tentative", "unavailable"]`. Empty
    /// when the calendar holds reminders only, or for source backends that
    /// report `EKCalendarEventAvailabilityNone`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    supported_event_availabilities: Vec<String>,
}

impl CalendarOutput {
    fn from_info(c: &crate::CalendarInfo) -> Self {
        Self {
            id: c.identifier.clone(),
            title: c.title.clone(),
            color: c
                .color
                .map(|(r, g, b, _)| CalendarColor::from_rgba(r, g, b).to_string())
                .unwrap_or_else(|| "none".into()),
            source: c.source.clone(),
            source_id: c.source_id.clone(),
            allows_modifications: c.allows_modifications,
            is_immutable: c.is_immutable,
            is_subscribed: c.is_subscribed,
            entity_types: c.allowed_entity_types.clone(),
            supported_event_availabilities: c.supported_event_availabilities.clone(),
        }
    }
}

#[derive(Serialize, JsonSchema)]
struct SourceOutput {
    id: String,
    title: String,
    source_type: String,
}

impl SourceOutput {
    fn from_info(s: &crate::SourceInfo) -> Self {
        Self {
            id: s.identifier.clone(),
            title: s.title.clone(),
            source_type: s.source_type.clone(),
        }
    }
}

// ============================================================================
// Shared Enums
// ============================================================================

/// Priority level for reminders.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    /// No priority (0)
    None,
    /// Low priority (9)
    Low,
    /// Medium priority (5)
    Medium,
    /// High priority (1) — also shows as "flagged" in Reminders.app
    High,
}

impl Priority {
    fn to_usize(&self) -> usize {
        match self {
            Priority::None => 0,
            Priority::Low => 9,
            Priority::Medium => 5,
            Priority::High => 1,
        }
    }

    fn label(v: usize) -> &'static str {
        match v {
            1..=4 => "high",
            5 => "medium",
            6..=9 => "low",
            _ => "none",
        }
    }
}

/// Item type discriminator for unified tools.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum ItemType {
    Reminder,
    Event,
}

// ============================================================================
// Inline Alarm & Recurrence Param Types
// ============================================================================

/// Alarm configuration for inline use in create/update.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct AlarmParam {
    /// Offset in seconds before the due date (negative = before, e.g., -600 = 10 minutes before)
    pub relative_offset: Option<f64>,
    /// Proximity trigger: "enter" or "leave" (for location-based alarms on reminders)
    pub proximity: Option<String>,
    /// Title of the location for geofenced alarms
    pub location_title: Option<String>,
    /// Latitude of the location
    pub latitude: Option<f64>,
    /// Longitude of the location
    pub longitude: Option<f64>,
    /// Geofence radius in meters (default: 100)
    pub radius: Option<f64>,
    /// Email address — if set, EventKit treats this as an email-type alarm
    /// (server-side notification for CalDAV calendars).
    pub email_address: Option<String>,
    /// Custom audio cue name — if set, EventKit treats this as an audio-type
    /// alarm. Macos sound names: "Glass", "Ping", "Pop", etc.
    pub sound_name: Option<String>,
    /// URL opened when the alarm fires — if set, EventKit treats this as a
    /// procedure-type alarm. Apple deprecated this property in macOS 10.9
    /// but it still functions. Strictly RFC 3986 validated.
    pub url: Option<String>,
}

/// Recurrence configuration for inline use in create/update.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct RecurrenceParam {
    /// Frequency: "daily", "weekly", "monthly", or "yearly"
    pub frequency: String,
    /// Repeat every N intervals (e.g., 2 = every 2 weeks). Default: 1
    #[serde(default = "default_interval")]
    #[schemars(with = "i64")]
    pub interval: usize,
    /// Days of the week (1=Sun, 2=Mon, ..., 7=Sat) for weekly/monthly rules
    #[schemars(with = "Option<Vec<i32>>")]
    pub days_of_week: Option<Vec<u8>>,
    /// Days of the month (1..=31, or negatives counting from the end) for monthly rules
    pub days_of_month: Option<Vec<i32>>,
    /// Months of the year (1..=12) for yearly rules. e.g. `[3]` = every March
    pub months_of_year: Option<Vec<i32>>,
    /// Weeks of the year (1..=53, or negatives counting from the end) for yearly rules
    pub weeks_of_year: Option<Vec<i32>>,
    /// Days of the year (1..=366, or negatives counting from the end) for yearly rules
    pub days_of_year: Option<Vec<i32>>,
    /// Set positions — filter applied after other fields. e.g. with
    /// `frequency=monthly, days_of_week=[2], set_positions=[1]` = "first Monday
    /// of every month". Negative values count from the end.
    pub set_positions: Option<Vec<i32>>,
    /// End after this many occurrences (mutually exclusive with end_date)
    #[schemars(with = "Option<i64>")]
    pub end_after_count: Option<usize>,
    /// End on this date in format 'YYYY-MM-DD' (mutually exclusive with end_after_count)
    pub end_date: Option<String>,
}

// ============================================================================
// Request/Response Types for Tools
// ============================================================================

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct ListRemindersRequest {
    /// If true, show all reminders including completed ones. Default: false.
    /// Ignored when any `completed_*` filter is supplied (those imply
    /// completed-only).
    #[serde(default)]
    pub show_completed: bool,
    /// Optional: Filter to a specific reminder list by name
    pub list_name: Option<String>,
    /// Only return *incomplete* reminders whose due date is at or after this
    /// timestamp. Format: 'YYYY-MM-DD' or 'YYYY-MM-DD HH:MM'.
    pub due_after: Option<String>,
    /// Only return *incomplete* reminders whose due date is before this
    /// timestamp. Pair with `due_after` for a window.
    pub due_before: Option<String>,
    /// Only return *completed* reminders whose completion date is at or after
    /// this timestamp. Presence of this field implies completed-only mode.
    pub completed_after: Option<String>,
    /// Only return *completed* reminders whose completion date is before
    /// this timestamp. Presence of this field implies completed-only mode.
    pub completed_before: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[allow(non_snake_case)]
pub struct CreateReminderRequest {
    /// The title/name of the reminder
    pub title: String,
    /// The name of the reminder list to add to (REQUIRED - use list_reminder_lists to see available lists)
    pub list_name: String,
    /// Optional notes/description for the reminder
    pub notes: Option<String>,
    /// Priority: "none", "low", "medium", "high" (high = flagged)
    pub priority: Option<Priority>,
    /// Optional due date in format 'YYYY-MM-DD' or 'YYYY-MM-DD HH:MM'. If only time 'HH:MM' is given, today's date is used.
    pub due_date: Option<String>,
    /// Optional start date when to begin working (format: 'YYYY-MM-DD' or 'YYYY-MM-DD HH:MM')
    pub start_date: Option<String>,
    /// Optional IANA timezone applied specifically to the due date
    /// (e.g. "America/Los_Angeles"). Lets the reminder fire at the same
    /// wall-clock time regardless of the device's current zone.
    pub due_date_timezone: Option<String>,
    /// Optional geofence — attaches a location-based alarm with the given
    /// title/lat/lng/radius/proximity. Triggers a Location permission
    /// prompt the first time it's used. This is the only location path
    /// iCloud Reminders honors; the plain `location` and rich-link
    /// `structuredLocation` properties on `EKReminder` are silently
    /// dropped by the iCloud daemon and so aren't exposed here.
    pub geofence: Option<GeofenceInput>,
    /// Optional alarms (replaces all existing). Each alarm can be time-based or location-based.
    pub alarms: Option<Vec<AlarmParam>>,
    /// Optional recurrence rule (replaces existing)
    pub recurrence: Option<RecurrenceParam>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[allow(non_snake_case)]
pub struct UpdateReminderRequest {
    /// The unique identifier of the reminder to update
    pub reminder_id: String,
    /// The name of the reminder list to move this reminder to
    pub list_name: Option<String>,
    /// New title for the reminder
    pub title: Option<String>,
    /// New notes for the reminder
    pub notes: Option<String>,
    /// Mark as completed (true) or incomplete (false)
    pub completed: Option<bool>,
    /// Priority: "none", "low", "medium", "high" (high = flagged)
    pub priority: Option<Priority>,
    /// New due date in format 'YYYY-MM-DD' or 'YYYY-MM-DD HH:MM'. Set to empty string to clear.
    pub due_date: Option<String>,
    /// New start date. Set to empty string to clear.
    pub start_date: Option<String>,
    /// IANA timezone applied specifically to the due date
    /// (e.g. "America/Los_Angeles"). Set to empty string to clear.
    pub due_date_timezone: Option<String>,
    /// Explicit completion timestamp. Setting this implicitly marks the
    /// reminder completed; setting it to `""` clears completion (and the
    /// reminder becomes incomplete). Apple's `setCompletionDate:` is
    /// the authoritative completion toggle, so this wins over `completed`
    /// when both are provided. ISO format: 'YYYY-MM-DD' or
    /// 'YYYY-MM-DDTHH:MM:SS±HH:MM'.
    pub completion_date: Option<String>,
    /// Alarms (replaces all existing when provided). Pass empty array to clear.
    pub alarms: Option<Vec<AlarmParam>>,
    /// Recurrence rule (replaces existing when provided). Omit to keep, set frequency to "" to clear.
    pub recurrence: Option<RecurrenceParam>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CreateReminderListRequest {
    /// The name of the new reminder list to create
    pub name: String,
}

/// Predefined colors for calendars and reminder lists.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CalendarColor {
    Red,
    Orange,
    Yellow,
    Green,
    Blue,
    Purple,
    Brown,
    Pink,
    Teal,
}

impl CalendarColor {
    fn to_rgba(&self) -> (f64, f64, f64, f64) {
        match self {
            CalendarColor::Red => (1.0, 0.231, 0.188, 1.0),
            CalendarColor::Orange => (1.0, 0.584, 0.0, 1.0),
            CalendarColor::Yellow => (1.0, 0.8, 0.0, 1.0),
            CalendarColor::Green => (0.298, 0.851, 0.392, 1.0),
            CalendarColor::Blue => (0.0, 0.478, 1.0, 1.0),
            CalendarColor::Purple => (0.686, 0.322, 0.871, 1.0),
            CalendarColor::Brown => (0.635, 0.518, 0.369, 1.0),
            CalendarColor::Pink => (1.0, 0.176, 0.333, 1.0),
            CalendarColor::Teal => (0.353, 0.784, 0.98, 1.0),
        }
    }

    /// Find the closest named color for an RGBA value.
    fn from_rgba(r: f64, g: f64, b: f64) -> &'static str {
        let colors: &[(&str, (f64, f64, f64))] = &[
            ("red", (1.0, 0.231, 0.188)),
            ("orange", (1.0, 0.584, 0.0)),
            ("yellow", (1.0, 0.8, 0.0)),
            ("green", (0.298, 0.851, 0.392)),
            ("blue", (0.0, 0.478, 1.0)),
            ("purple", (0.686, 0.322, 0.871)),
            ("brown", (0.635, 0.518, 0.369)),
            ("pink", (1.0, 0.176, 0.333)),
            ("teal", (0.353, 0.784, 0.98)),
        ];
        colors
            .iter()
            .min_by(|(_, a), (_, b_)| {
                let da = (a.0 - r).powi(2) + (a.1 - g).powi(2) + (a.2 - b).powi(2);
                let db = (b_.0 - r).powi(2) + (b_.1 - g).powi(2) + (b_.2 - b).powi(2);
                da.partial_cmp(&db).unwrap()
            })
            .map(|(name, _)| *name)
            .unwrap_or("blue")
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct UpdateReminderListRequest {
    /// The unique identifier of the reminder list to update
    pub list_id: String,
    /// New name for the list (optional)
    pub name: Option<String>,
    /// Color for the list (optional). Use a color name or custom hex.
    pub color: Option<CalendarColor>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct UpdateEventCalendarRequest {
    /// The unique identifier of the event calendar to update
    pub calendar_id: String,
    /// New name for the calendar (optional)
    pub name: Option<String>,
    /// Color for the calendar (optional). Use a color name or custom hex.
    pub color: Option<CalendarColor>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct DeleteReminderListRequest {
    /// The unique identifier of the reminder list to delete
    pub list_id: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ReminderIdRequest {
    /// The unique identifier of the reminder
    pub reminder_id: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ListEventsRequest {
    /// Number of days from today to include (default: 1 for today only)
    #[serde(default = "default_days")]
    pub days: i64,
    /// Optional: Filter to a specific calendar by ID (use list_calendars to get IDs)
    pub calendar_id: Option<String>,
}

fn default_days() -> i64 {
    1
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[allow(non_snake_case)]
pub struct CreateEventRequest {
    /// The title of the event
    pub title: String,
    /// Start date/time in format 'YYYY-MM-DD HH:MM' or 'YYYY-MM-DD' for all-day events
    pub start: String,
    /// End date/time in format 'YYYY-MM-DD HH:MM'. If not specified, uses duration_minutes.
    pub end: Option<String>,
    /// Duration in minutes (default: 60). Used if end is not specified.
    #[serde(default = "default_duration")]
    pub duration_minutes: i64,
    /// Optional notes/description for the event
    pub notes: Option<String>,
    /// Optional location for the event
    pub location: Option<String>,
    /// Optional: The name of the calendar to add to
    pub calendar_name: Option<String>,
    /// Whether this is an all-day event
    #[serde(default)]
    pub all_day: bool,
    /// Optional URL to associate with the event
    pub URL: Option<String>,
    /// Optional availability: "busy" (default), "free", "tentative",
    /// "unavailable". Controls how the event shows on the timeline.
    pub availability: Option<String>,
    /// Optional structured location (title + lat/lng + radius). Enables
    /// travel-time, map preview, and "leave at" suggestions in Calendar.app.
    pub structured_location: Option<StructuredLocationInput>,
    /// Optional alarms (replaces all existing). Time-based only for events.
    pub alarms: Option<Vec<AlarmParam>>,
    /// Optional recurrence rule
    pub recurrence: Option<RecurrenceParam>,
}

fn default_duration() -> i64 {
    60
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct EventIdRequest {
    /// The unique identifier of the event
    pub event_id: String,
    /// Edit scope for recurring events: "this" (default) or "future".
    /// Mirrors `EKSpan::ThisEvent` / `EKSpan::FutureEvents`.
    pub span: Option<String>,
    /// Deprecated alias for `span`. If true, equivalent to `span: "future"`.
    /// New callers should use `span`; kept for back-compat.
    #[serde(default)]
    pub affect_future: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[allow(non_snake_case)]
pub struct UpdateEventRequest {
    /// The unique identifier of the event to update
    pub event_id: String,
    /// New title for the event
    pub title: Option<String>,
    /// New notes for the event (empty string clears)
    pub notes: Option<String>,
    /// New location for the event (empty string clears)
    pub location: Option<String>,
    /// New start date/time in format 'YYYY-MM-DD HH:MM'
    pub start: Option<String>,
    /// New end date/time in format 'YYYY-MM-DD HH:MM'
    pub end: Option<String>,
    /// Toggle all-day flag.
    pub all_day: Option<bool>,
    /// Move to another calendar by name.
    pub calendar_name: Option<String>,
    /// URL to associate (empty string clears)
    pub URL: Option<String>,
    /// New availability: "busy" | "free" | "tentative" | "unavailable".
    pub availability: Option<String>,
    /// New structured location; pass `null` to clear.
    pub structured_location: Option<StructuredLocationInput>,
    /// Edit scope for recurring events: "this" (default — only this
    /// occurrence) or "future" (this and every later occurrence).
    pub span: Option<String>,
    /// Alarms (replaces all existing when provided). Pass empty array to clear.
    pub alarms: Option<Vec<AlarmParam>>,
    /// Recurrence rule (replaces existing when provided)
    pub recurrence: Option<RecurrenceParam>,
}

// ============================================================================
// Batch Operation Request Types
// ============================================================================

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BatchDeleteRequest {
    /// Whether these are "reminder" or "event" items
    pub item_type: ItemType,
    /// List of item IDs to delete
    pub item_ids: Vec<String>,
    /// For recurring events: if true, delete this and all future occurrences
    #[serde(default)]
    pub affect_future: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BatchMoveRequest {
    /// List of reminder IDs to move
    pub reminder_ids: Vec<String>,
    /// The name of the destination reminder list
    pub destination_list_name: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BatchUpdateItem {
    /// The unique identifier of the item to update
    pub item_id: String,
    /// New title
    pub title: Option<String>,
    /// New notes
    pub notes: Option<String>,
    /// Mark completed (reminders only)
    pub completed: Option<bool>,
    /// Priority (reminders only)
    pub priority: Option<Priority>,
    /// Due date (reminders only)
    pub due_date: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BatchUpdateRequest {
    /// Whether these are "reminder" or "event" items
    pub item_type: ItemType,
    /// List of updates to apply
    pub updates: Vec<BatchUpdateItem>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct SearchRequest {
    /// Text to search for in titles and notes (case-insensitive)
    pub query: String,
    /// Whether to search "reminder" or "event" items. If omitted, searches both.
    pub item_type: Option<ItemType>,
    /// For reminders: if true, also search completed reminders. Default: false
    #[serde(default)]
    pub include_completed: bool,
    /// For events: number of days from today to search (default: 30)
    #[serde(default = "default_search_days")]
    pub days: i64,
}

fn default_search_days() -> i64 {
    30
}

fn default_interval() -> usize {
    1
}

// ============================================================================
// Prompt Argument Types
// ============================================================================

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ListRemindersPromptArgs {
    /// Name of the reminder list to show. If not provided, shows all lists.
    #[serde(default)]
    pub list_name: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct MoveReminderPromptArgs {
    /// The unique identifier of the reminder to move
    pub reminder_id: String,
    /// The name of the destination reminder list
    pub destination_list: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct CreateReminderPromptArgs {
    /// Title of the reminder
    pub title: String,
    /// Detailed notes/context for the reminder
    #[serde(default)]
    pub notes: Option<String>,
    /// Name of the reminder list to add to
    #[serde(default)]
    pub list_name: Option<String>,
    /// Priority (0 = none, 1-4 = high, 5 = medium, 6-9 = low)
    #[serde(default)]
    #[schemars(with = "Option<i32>")]
    pub priority: Option<u8>,
    /// Due date in format "YYYY-MM-DD" or "YYYY-MM-DD HH:MM"
    #[serde(default)]
    pub due_date: Option<String>,
}

// ============================================================================
// EventKit MCP Server
// ============================================================================

/// EventKit MCP Server - provides access to macOS Calendar and Reminders.
///
/// EventKit objects (`Retained<EKEventStore>` and its managers) are `!Send + !Sync`,
/// but every handler in this module keeps those values stack-local and never holds
/// one across an `.await`. That makes the generated handler futures `Send`, so the
/// server can run on a normal multi-thread tokio runtime without rmcp's `local`
/// feature. New handlers MUST preserve this invariant — if you need async work,
/// wrap the synchronous EventKit calls in `tokio::task::spawn_blocking` so the
/// `!Send` value lives entirely inside the blocking closure.
pub struct EventKitServer {}

impl Default for EventKitServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse a date string in format "YYYY-MM-DD" or "YYYY-MM-DD HH:MM"
fn parse_datetime(s: &str) -> Result<DateTime<Local>, String> {
    // Try parsing with time first
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return Local
            .from_local_datetime(&dt)
            .single()
            .ok_or_else(|| "Invalid local datetime".to_string());
    }

    // Try parsing date only
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = date
            .and_hms_opt(0, 0, 0)
            .ok_or_else(|| "Invalid date".to_string())?;
        return Local
            .from_local_datetime(&dt)
            .single()
            .ok_or_else(|| "Invalid local datetime".to_string());
    }

    Err("Invalid date format. Use 'YYYY-MM-DD' or 'YYYY-MM-DD HH:MM'".to_string())
}

#[tool_router]
#[allow(non_snake_case)]
impl EventKitServer {
    pub fn new() -> Self {
        Self {}
    }

    // ========================================================================
    // Authorization
    // ========================================================================

    #[tool(
        description = "Check macOS permission status for Reminders and Calendar without requesting access. Use this to diagnose authorization problems before calling other tools — it never triggers a consent dialog."
    )]
    async fn auth_status(&self) -> Result<Json<AuthStatusOutput>, McpError> {
        let reminders = RemindersManager::authorization_status();
        let events = EventsManager::authorization_status();
        Ok(Json(AuthStatusOutput {
            reminders: auth_status_str(reminders),
            events: auth_status_str(events),
            remediation: auth_remediation(reminders, events),
        }))
    }

    #[tool(
        description = "Trigger the macOS consent dialog for Reminders or Calendar access. The first call shows the system prompt; subsequent calls return the cached status. Blocks until the user responds. Use `entity` = \"reminder\" or \"event\"."
    )]
    async fn request_access(
        &self,
        Parameters(params): Parameters<RequestAccessRequest>,
    ) -> Result<Json<RequestAccessOutput>, McpError> {
        match params.entity {
            AccessEntity::Reminder => {
                let manager = RemindersManager::new();
                let granted = manager.request_access().map_err(|e| mcp_err(&e))?;
                Ok(Json(RequestAccessOutput {
                    granted,
                    status: auth_status_str(RemindersManager::authorization_status()),
                }))
            }
            AccessEntity::Event => {
                let manager = EventsManager::new();
                let granted = manager.request_access().map_err(|e| mcp_err(&e))?;
                Ok(Json(RequestAccessOutput {
                    granted,
                    status: auth_status_str(EventsManager::authorization_status()),
                }))
            }
        }
    }

    // ========================================================================
    // Reminders Tools
    // ========================================================================

    #[tool(description = "List all reminder lists (calendars) available in macOS Reminders.")]
    async fn list_reminder_lists(&self) -> Result<Json<ListResponse<CalendarOutput>>, McpError> {
        let manager = RemindersManager::new();
        match manager.list_calendars() {
            Ok(lists) => {
                let items: Vec<_> = lists.iter().map(CalendarOutput::from_info).collect();
                Ok(Json(ListResponse {
                    count: items.len(),
                    items,
                }))
            }
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(
        description = "List reminders from macOS Reminders app. Filters: `show_completed` toggles inclusion of completed items; `list_name` restricts to one list; `due_after`/`due_before` window incomplete reminders by their due date; `completed_after`/`completed_before` window completed reminders by their completion date. When any `completed_*` filter is supplied, results are completed-only regardless of `show_completed`."
    )]
    async fn list_reminders(
        &self,
        Parameters(params): Parameters<ListRemindersRequest>,
    ) -> Result<Json<ListResponse<ReminderOutput>>, McpError> {
        let manager = RemindersManager::new();

        fn parse_opt_date(
            label: &str,
            s: &Option<String>,
        ) -> Result<Option<DateTime<Local>>, McpError> {
            s.as_deref()
                .map(parse_datetime_or_time)
                .transpose()
                .map_err(|e| mcp_invalid(format!("Error parsing {label}: {e}")))
        }
        let due_after = parse_opt_date("due_after", &params.due_after)?;
        let due_before = parse_opt_date("due_before", &params.due_before)?;
        let completed_after = parse_opt_date("completed_after", &params.completed_after)?;
        let completed_before = parse_opt_date("completed_before", &params.completed_before)?;

        // Resolve list_name into the slice the manager expects.
        let list_name_owned = params.list_name.clone();
        let calendar_titles: Option<Vec<&str>> = list_name_owned.as_ref().map(|n| vec![n.as_str()]);
        let calendar_titles_ref: Option<&[&str]> = calendar_titles.as_deref();

        let reminders = if completed_after.is_some() || completed_before.is_some() {
            manager.fetch_completed_reminders_in_range(
                completed_after,
                completed_before,
                calendar_titles_ref,
            )
        } else if due_after.is_some() || due_before.is_some() {
            manager.fetch_incomplete_reminders_in_due_range(
                due_after,
                due_before,
                calendar_titles_ref,
            )
        } else if params.show_completed {
            // Fall back to all-reminders + post-filter for backward compat —
            // EventKit has no "all reminders in calendars" predicate that
            // also includes completed by default; fetch_all delegates to
            // fetch_reminders(None) which uses predicateForRemindersInCalendars.
            manager.fetch_reminders(calendar_titles_ref)
        } else {
            manager.fetch_incomplete_reminders_in_due_range(None, None, calendar_titles_ref)
        };

        match reminders {
            Ok(items) => {
                let items: Vec<_> = items
                    .iter()
                    .map(ReminderOutput::from_item_summary)
                    .collect();
                Ok(Json(ListResponse {
                    count: items.len(),
                    items,
                }))
            }
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(
        description = "Create a new reminder in macOS Reminders. You MUST specify which list to add it to (use list_reminder_lists first to see available lists). Inline configuration: alarms (time-based or proximity-based via `geofence`), recurrence, due/start dates, IANA timezone for the due date. NB: `URL`, free-text `location`, and `structured_location` are intentionally absent on the reminder surface — iCloud Reminders silently drops those mutations. Use `set_reminder_geofence` for location-based reminders (the iCloud-honored path); for events those fields are first-class via `create_event`. Tags (the Reminders.app Tag Sidebar) are an iCloud server-side feature not reachable through EventKit at all."
    )]
    async fn create_reminder(
        &self,
        Parameters(params): Parameters<CreateReminderRequest>,
    ) -> Result<Json<ReminderOutput>, McpError> {
        let manager = RemindersManager::new();

        // Validate the list exists
        let calendar_title = match manager.list_calendars() {
            Ok(lists) => {
                if let Some(cal) = lists.iter().find(|c| c.title == params.list_name) {
                    cal.title.clone()
                } else {
                    let available: Vec<_> = lists.iter().map(|c| c.title.as_str()).collect();
                    return Err(mcp_invalid(format!(
                        "List '{}' not found. Available lists: {}",
                        params.list_name,
                        available.join(", ")
                    )));
                }
            }
            Err(e) => {
                return Err(mcp_invalid(format!("Error listing calendars: {e}")));
            }
        };

        let due_date = match params
            .due_date
            .as_deref()
            .map(parse_datetime_or_time)
            .transpose()
        {
            Ok(v) => v,
            Err(e) => return Err(mcp_invalid(format!("Error parsing due_date: {e}"))),
        };
        let start_date = match params.start_date.as_deref().map(parse_datetime).transpose() {
            Ok(v) => v,
            Err(e) => return Err(mcp_invalid(format!("Error parsing start_date: {e}"))),
        };

        let priority = params.priority.as_ref().map(Priority::to_usize);

        match manager.create_reminder(&crate::ReminderDraft {
            title: &params.title,
            notes: params.notes.as_deref(),
            calendar_title: Some(&calendar_title),
            priority,
            due_date,
            start_date,
            due_date_timezone: params.due_date_timezone.as_deref(),
            ..Default::default()
        }) {
            Ok(reminder) => {
                let id = reminder.identifier.clone();
                if let Some(alarms) = &params.alarms {
                    apply_alarms_reminder(&manager, &id, alarms);
                }
                if let Some(g) = &params.geofence {
                    let proximity = match g.proximity {
                        GeofenceProximity::Enter => crate::AlarmProximity::Enter,
                        GeofenceProximity::Leave => crate::AlarmProximity::Leave,
                    };
                    let sl = crate::StructuredLocation {
                        title: g.title.clone(),
                        latitude: g.latitude,
                        longitude: g.longitude,
                        radius: g.radius_meters,
                    };
                    if let Err(e) = manager.set_geofence(&id, Some((&sl, proximity))) {
                        return Err(mcp_err(&e));
                    }
                }
                if let Some(recurrence) = &params.recurrence
                    && let Ok(rule) = parse_recurrence_param(recurrence)
                {
                    let _ = manager.set_recurrence_rule(&id, &rule);
                }
                let updated = manager.get_reminder(&id).unwrap_or(reminder);
                Ok(Json(ReminderOutput::from_item(&updated, &manager)))
            }
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(
        description = "Update an existing reminder. All fields are optional; only the ones you supply are written. Inline edits: title, notes, completed, priority, due/start date (empty string clears), due-date IANA timezone (empty string clears), completion_date (empty string clears and marks incomplete), alarms (replaces all when supplied), recurrence, list move. NB: `URL`/`location`/`structured_location` are absent — see `create_reminder`."
    )]
    async fn update_reminder(
        &self,
        Parameters(params): Parameters<UpdateReminderRequest>,
    ) -> Result<Json<ReminderOutput>, McpError> {
        let manager = RemindersManager::new();

        // Parse due date: Some("") means clear, Some(date) means set, None means no change
        let due_date = match &params.due_date {
            Some(due_str) if due_str.is_empty() => Some(None),
            Some(due_str) => match parse_datetime_or_time(due_str) {
                Ok(dt) => Some(Some(dt)),
                Err(e) => return Err(mcp_invalid(format!("Error parsing due_date: {e}"))),
            },
            None => None,
        };

        let start_date = match &params.start_date {
            Some(start_str) if start_str.is_empty() => Some(None),
            Some(start_str) => match parse_datetime(start_str) {
                Ok(dt) => Some(Some(dt)),
                Err(e) => return Err(mcp_invalid(format!("Error parsing start_date: {e}"))),
            },
            None => None,
        };

        if let Some(ref list_name) = params.list_name {
            match manager.list_calendars() {
                Ok(lists) => {
                    if !lists.iter().any(|c| &c.title == list_name) {
                        let available: Vec<_> = lists.iter().map(|c| c.title.as_str()).collect();
                        return Err(mcp_invalid(format!(
                            "List '{}' not found. Available lists: {}",
                            list_name,
                            available.join(", ")
                        )));
                    }
                }
                Err(e) => return Err(mcp_invalid(format!("Error: {e}"))),
            }
        }

        let priority = params.priority.as_ref().map(Priority::to_usize);

        // Map each Option<String> ("" means clear, otherwise set) to the
        // Option<Option<&str>> patch encoding (Some(None) = clear).
        fn opt_patch(s: &Option<String>) -> Option<Option<&str>> {
            s.as_ref()
                .map(|v| if v.is_empty() { None } else { Some(v.as_str()) })
        }
        let tz_patch = opt_patch(&params.due_date_timezone);

        // completion_date: same "" = clear, set = set, omitted = no change.
        let completion_date_patch: Option<Option<DateTime<Local>>> = match &params.completion_date {
            None => None,
            Some(s) if s.is_empty() => Some(None),
            Some(s) => match parse_datetime(s) {
                Ok(dt) => Some(Some(dt)),
                Err(e) => return Err(mcp_invalid(format!("Error parsing completion_date: {e}"))),
            },
        };

        match manager.update_reminder(
            &params.reminder_id,
            &crate::ReminderPatch {
                title: params.title.as_deref(),
                notes: params.notes.as_deref(),
                completed: params.completed,
                priority,
                due_date,
                start_date,
                calendar_title: params.list_name.as_deref(),
                due_date_timezone: tz_patch,
                completion_date: completion_date_patch,
                ..Default::default()
            },
        ) {
            Ok(reminder) => {
                let id = reminder.identifier.clone();
                if let Some(alarms) = &params.alarms {
                    apply_alarms_reminder(&manager, &id, alarms);
                }
                if let Some(recurrence) = &params.recurrence {
                    if recurrence.frequency.is_empty() {
                        let _ = manager.remove_recurrence_rules(&id);
                    } else if let Ok(rule) = parse_recurrence_param(recurrence) {
                        let _ = manager.set_recurrence_rule(&id, &rule);
                    }
                }
                let updated = manager.get_reminder(&id).unwrap_or(reminder);
                Ok(Json(ReminderOutput::from_item(&updated, &manager)))
            }
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(description = "Create a new reminder list (calendar for reminders).")]
    async fn create_reminder_list(
        &self,
        Parameters(params): Parameters<CreateReminderListRequest>,
    ) -> Result<Json<CalendarOutput>, McpError> {
        let manager = RemindersManager::new();
        match manager.create_calendar(&params.name) {
            Ok(cal) => Ok(Json(CalendarOutput::from_info(&cal))),
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(description = "Update a reminder list — change name and/or color.")]
    async fn update_reminder_list(
        &self,
        Parameters(params): Parameters<UpdateReminderListRequest>,
    ) -> Result<Json<CalendarOutput>, McpError> {
        let manager = RemindersManager::new();
        let color_rgba = params.color.as_ref().map(CalendarColor::to_rgba);
        match manager.update_calendar(&params.list_id, params.name.as_deref(), color_rgba) {
            Ok(cal) => Ok(Json(CalendarOutput::from_info(&cal))),
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(
        description = "Delete a reminder list. WARNING: This will delete all reminders in the list!"
    )]
    async fn delete_reminder_list(
        &self,
        Parameters(params): Parameters<DeleteReminderListRequest>,
    ) -> Result<Json<DeletedResponse>, McpError> {
        let manager = RemindersManager::new();
        match manager.delete_calendar(&params.list_id) {
            Ok(_) => Ok(Json(DeletedResponse { id: params.list_id })),
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(description = "Mark a reminder as completed.")]
    async fn complete_reminder(
        &self,
        Parameters(params): Parameters<ReminderIdRequest>,
    ) -> Result<Json<ReminderOutput>, McpError> {
        let manager = RemindersManager::new();
        match manager.complete_reminder(&params.reminder_id) {
            Ok(_) => {
                let r = manager.get_reminder(&params.reminder_id);
                match r {
                    Ok(r) => Ok(Json(ReminderOutput::from_item(&r, &manager))),
                    Err(e) => Err(mcp_err(&e)),
                }
            }
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(description = "Mark a reminder as not completed (uncomplete it).")]
    async fn uncomplete_reminder(
        &self,
        Parameters(params): Parameters<ReminderIdRequest>,
    ) -> Result<Json<ReminderOutput>, McpError> {
        let manager = RemindersManager::new();
        match manager.uncomplete_reminder(&params.reminder_id) {
            Ok(_) => {
                let r = manager.get_reminder(&params.reminder_id);
                match r {
                    Ok(r) => Ok(Json(ReminderOutput::from_item(&r, &manager))),
                    Err(e) => Err(mcp_err(&e)),
                }
            }
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(description = "Get a single reminder by its unique identifier.")]
    async fn get_reminder(
        &self,
        Parameters(params): Parameters<ReminderIdRequest>,
    ) -> Result<Json<ReminderOutput>, McpError> {
        let manager = RemindersManager::new();
        match manager.get_reminder(&params.reminder_id) {
            Ok(r) => Ok(Json(ReminderOutput::from_item(&r, &manager))),
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(description = "Delete a reminder from macOS Reminders.")]
    async fn delete_reminder(
        &self,
        Parameters(params): Parameters<ReminderIdRequest>,
    ) -> Result<Json<DeletedResponse>, McpError> {
        let manager = RemindersManager::new();
        match manager.delete_reminder(&params.reminder_id) {
            Ok(_) => Ok(Json(DeletedResponse {
                id: params.reminder_id,
            })),
            Err(e) => Err(mcp_err(&e)),
        }
    }

    // ------------------------------------------------------------------------
    // Reminder posture setters (one field at a time, for fix-ups)
    //
    // NB: URL, plain location, and rich structuredLocation setters are
    // intentionally not exposed for reminders — iCloud silently drops those
    // mutations even though the EventKit APIs accept them. The geofence path
    // (location-based alarm) is the only iCloud-honored location write.
    // For events those same fields are first-class — see create_event /
    // update_event.
    // ------------------------------------------------------------------------

    #[tool(
        description = "Set or clear the timezone applied specifically to the reminder's due date (separate from the item-level timezone). Use an IANA zone like \"America/Los_Angeles\". Pass `timezone: null` or `\"\"` to clear."
    )]
    async fn set_reminder_due_timezone(
        &self,
        Parameters(params): Parameters<SetReminderDueTimezoneRequest>,
    ) -> Result<Json<ReminderOutput>, McpError> {
        let manager = RemindersManager::new();
        let tz = params.timezone.as_deref().filter(|t| !t.is_empty());
        manager
            .set_due_date_timezone(&params.reminder_id, tz)
            .map_err(|e| mcp_err(&e))?;
        let item = manager
            .get_reminder(&params.reminder_id)
            .map_err(|e| mcp_err(&e))?;
        Ok(Json(ReminderOutput::from_item(&item, &manager)))
    }

    #[tool(
        description = "Attach (or clear) a geofence on a reminder. Implemented as a location-based alarm — \"remind me when I arrive at/leave this place\". Triggers a Location permission prompt the first time. Omit `geofence` to clear any existing geofence."
    )]
    async fn set_reminder_geofence(
        &self,
        Parameters(params): Parameters<SetReminderGeofenceRequest>,
    ) -> Result<Json<ReminderOutput>, McpError> {
        let manager = RemindersManager::new();
        let owned = params.geofence.as_ref().map(|g| {
            let proximity = match g.proximity {
                GeofenceProximity::Enter => crate::AlarmProximity::Enter,
                GeofenceProximity::Leave => crate::AlarmProximity::Leave,
            };
            (
                crate::StructuredLocation {
                    title: g.title.clone(),
                    latitude: g.latitude,
                    longitude: g.longitude,
                    radius: g.radius_meters,
                },
                proximity,
            )
        });
        let geofence_ref = owned.as_ref().map(|(s, p)| (s, *p));
        manager
            .set_geofence(&params.reminder_id, geofence_ref)
            .map_err(|e| mcp_err(&e))?;
        let item = manager
            .get_reminder(&params.reminder_id)
            .map_err(|e| mcp_err(&e))?;
        Ok(Json(ReminderOutput::from_item(&item, &manager)))
    }

    // ========================================================================
    // Calendar/Events Tools
    // ========================================================================

    #[tool(description = "List all calendars available in macOS Calendar app.")]
    async fn list_calendars(&self) -> Result<Json<ListResponse<CalendarOutput>>, McpError> {
        let manager = EventsManager::new();
        match manager.list_calendars() {
            Ok(cals) => {
                let items: Vec<_> = cals.iter().map(CalendarOutput::from_info).collect();
                Ok(Json(ListResponse {
                    count: items.len(),
                    items,
                }))
            }
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(
        description = "Return the calendar that will be used by `create_event` when no `calendar_name` is supplied. Mirrors `EKEventStore.defaultCalendarForNewEvents`."
    )]
    async fn get_default_event_calendar(&self) -> Result<Json<CalendarOutput>, McpError> {
        let manager = EventsManager::new();
        match manager.default_calendar() {
            Ok(cal) => Ok(Json(CalendarOutput::from_info(&cal))),
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(
        description = "Set an event's availability — controls how the event shows on the timeline. Use \"busy\" (default), \"free\", \"tentative\", or \"unavailable\". Always applies to just this occurrence (per-instance attribute)."
    )]
    async fn set_event_availability(
        &self,
        Parameters(params): Parameters<SetEventAvailabilityRequest>,
    ) -> Result<Json<EventOutput>, McpError> {
        let manager = EventsManager::new();
        let availability = parse_availability(&params.availability).map_err(mcp_invalid)?;
        manager
            .set_event_availability(&params.event_id, availability)
            .map_err(|e| mcp_err(&e))?;
        let item = manager
            .get_event(&params.event_id)
            .map_err(|e| mcp_err(&e))?;
        Ok(Json(EventOutput::from_item(&item, &manager)))
    }

    #[tool(
        description = "List calendar events. By default shows today's events. Can specify a date range."
    )]
    async fn list_events(
        &self,
        Parameters(params): Parameters<ListEventsRequest>,
    ) -> Result<Json<ListResponse<EventOutput>>, McpError> {
        let manager = EventsManager::new();

        let events = if params.days == 1 {
            manager.fetch_today_events()
        } else {
            let start = Local::now();
            let end = start + Duration::days(params.days);
            manager.fetch_events(start, end, None)
        };

        match events {
            Ok(items) => {
                let filtered: Vec<_> = if let Some(ref cal_id) = params.calendar_id {
                    items
                        .into_iter()
                        .filter(|e| e.calendar_id.as_deref() == Some(cal_id.as_str()))
                        .collect()
                } else {
                    items
                };
                let items: Vec<_> = filtered
                    .iter()
                    .map(EventOutput::from_item_summary)
                    .collect();
                Ok(Json(ListResponse {
                    count: items.len(),
                    items,
                }))
            }
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(
        description = "Create a new calendar event in macOS Calendar. Inline configuration: title, start/end (or duration), location, calendar, all-day flag, URL, alarms (time-based; events don't support proximity alarms), recurrence."
    )]
    async fn create_event(
        &self,
        Parameters(params): Parameters<CreateEventRequest>,
    ) -> Result<Json<EventOutput>, McpError> {
        let manager = EventsManager::new();

        let start = match parse_datetime(&params.start) {
            Ok(dt) => dt,
            Err(e) => return Err(mcp_invalid(format!("Error: {e}"))),
        };

        let end = if let Some(end_str) = &params.end {
            match parse_datetime(end_str) {
                Ok(dt) => dt,
                Err(e) => return Err(mcp_invalid(format!("Error: {e}"))),
            }
        } else {
            start + Duration::minutes(params.duration_minutes)
        };

        let availability = params
            .availability
            .as_deref()
            .map(parse_availability)
            .transpose()
            .map_err(mcp_invalid)?;

        let sl_owned = params
            .structured_location
            .as_ref()
            .map(|s| crate::StructuredLocation {
                title: s.title.clone(),
                latitude: s.latitude,
                longitude: s.longitude,
                radius: s.radius_meters,
            });

        match manager.create_event(&crate::EventDraft {
            title: &params.title,
            start: Some(start),
            end: Some(end),
            notes: params.notes.as_deref(),
            location: params.location.as_deref(),
            calendar_title: params.calendar_name.as_deref(),
            all_day: params.all_day,
            URL: params.URL.as_deref(),
            availability,
            structured_location: sl_owned.as_ref(),
        }) {
            Ok(event) => {
                let id = event.identifier.clone();
                if let Some(alarms) = &params.alarms {
                    apply_alarms_event(&manager, &id, alarms);
                }
                if let Some(recurrence) = &params.recurrence
                    && let Ok(rule) = parse_recurrence_param(recurrence)
                {
                    let _ = manager.set_event_recurrence_rule(&id, &rule);
                }
                let updated = manager.get_event(&id).unwrap_or(event);
                Ok(Json(EventOutput::from_item(&updated, &manager)))
            }
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(
        description = "Delete a calendar event. `span: \"this\" | \"future\"` controls recurring-event scope (default: \"this\"). The legacy boolean `affect_future` is still accepted as an alias for `span: \"future\"`."
    )]
    async fn delete_event(
        &self,
        Parameters(params): Parameters<EventIdRequest>,
    ) -> Result<Json<DeletedResponse>, McpError> {
        let manager = EventsManager::new();
        let span = parse_span(params.span.as_deref()).map_err(mcp_invalid)?;
        let affect_future = matches!(span, crate::EventSpan::Future) || params.affect_future;
        match manager.delete_event(&params.event_id, affect_future) {
            Ok(_) => Ok(Json(DeletedResponse {
                id: params.event_id,
            })),
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(description = "Get a single calendar event by its unique identifier.")]
    async fn get_event(
        &self,
        Parameters(params): Parameters<EventIdRequest>,
    ) -> Result<Json<EventOutput>, McpError> {
        let manager = EventsManager::new();
        match manager.get_event(&params.event_id) {
            Ok(e) => Ok(Json(EventOutput::from_item(&e, &manager))),
            Err(e) => Err(mcp_err(&e)),
        }
    }

    // ========================================================================
    // Event Calendar Management
    // ========================================================================

    #[tool(description = "Create a new calendar for events.")]
    async fn create_event_calendar(
        &self,
        Parameters(params): Parameters<CreateReminderListRequest>,
    ) -> Result<Json<CalendarOutput>, McpError> {
        let manager = EventsManager::new();
        match manager.create_event_calendar(&params.name) {
            Ok(cal) => Ok(Json(CalendarOutput::from_info(&cal))),
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(description = "Update an event calendar — change name and/or color.")]
    async fn update_event_calendar(
        &self,
        Parameters(params): Parameters<UpdateEventCalendarRequest>,
    ) -> Result<Json<CalendarOutput>, McpError> {
        let manager = EventsManager::new();

        let color_rgba = params.color.as_ref().map(CalendarColor::to_rgba);

        match manager.update_event_calendar(&params.calendar_id, params.name.as_deref(), color_rgba)
        {
            Ok(cal) => Ok(Json(CalendarOutput::from_info(&cal))),
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[tool(
        description = "Delete an event calendar. WARNING: This will delete all events in the calendar!"
    )]
    async fn delete_event_calendar(
        &self,
        Parameters(params): Parameters<DeleteReminderListRequest>,
    ) -> Result<Json<DeletedResponse>, McpError> {
        let manager = EventsManager::new();
        match manager.delete_event_calendar(&params.list_id) {
            Ok(()) => Ok(Json(DeletedResponse { id: params.list_id })),
            Err(e) => Err(mcp_err(&e)),
        }
    }

    // ========================================================================
    // Sources
    // ========================================================================

    #[tool(description = "List all available sources (accounts like iCloud, Local, Exchange).")]
    async fn list_sources(&self) -> Result<Json<ListResponse<SourceOutput>>, McpError> {
        let manager = RemindersManager::new();
        match manager.list_sources() {
            Ok(sources) => {
                let items: Vec<_> = sources.iter().map(SourceOutput::from_info).collect();
                Ok(Json(ListResponse {
                    count: items.len(),
                    items,
                }))
            }
            Err(e) => Err(mcp_err(&e)),
        }
    }

    // ========================================================================
    // Event Update Tool
    // ========================================================================

    #[tool(
        description = "Update an existing calendar event. All fields are optional; only those you supply are written. Inline edits: title, notes (empty clears), location (empty clears), start/end, all_day toggle, calendar move (`calendar_name`), URL (empty clears), availability, structured_location (null clears), alarms (replaces all), recurrence (empty frequency clears). `span: \"this\" | \"future\"` controls recurring-event edit scope; defaults to \"this\"."
    )]
    async fn update_event(
        &self,
        Parameters(params): Parameters<UpdateEventRequest>,
    ) -> Result<Json<EventOutput>, McpError> {
        let manager = EventsManager::new();

        let start = match params.start.as_ref().map(|s| parse_datetime(s)).transpose() {
            Ok(v) => v,
            Err(e) => return Err(mcp_invalid(format!("Error: {e}"))),
        };
        let end = match params.end.as_ref().map(|s| parse_datetime(s)).transpose() {
            Ok(v) => v,
            Err(e) => return Err(mcp_invalid(format!("Error: {e}"))),
        };

        // Empty-string-clears convention, same as reminders side.
        fn opt_patch(s: &Option<String>) -> Option<Option<&str>> {
            s.as_ref()
                .map(|v| if v.is_empty() { None } else { Some(v.as_str()) })
        }
        let notes_patch = opt_patch(&params.notes);
        let location_patch = opt_patch(&params.location);
        let url_patch = opt_patch(&params.URL);

        let availability = params
            .availability
            .as_deref()
            .map(parse_availability)
            .transpose()
            .map_err(mcp_invalid)?;

        let sl_owned = params
            .structured_location
            .as_ref()
            .map(|s| crate::StructuredLocation {
                title: s.title.clone(),
                latitude: s.latitude,
                longitude: s.longitude,
                radius: s.radius_meters,
            });
        // We only get Some(value) here — no clear path through serde because
        // null collapses into None for Option<T>. For an explicit clear,
        // callers can call update_event again with a future schema enhancement
        // (low-frequency need; matches reminder side).
        let sl_patch = sl_owned.as_ref().map(Some);

        let span = parse_span(params.span.as_deref()).map_err(mcp_invalid)?;

        match manager.update_event(
            &params.event_id,
            &crate::EventPatch {
                title: params.title.as_deref(),
                notes: notes_patch,
                location: location_patch,
                start,
                end,
                all_day: params.all_day,
                calendar_title: params.calendar_name.as_deref(),
                URL: url_patch,
                availability,
                structured_location: sl_patch,
                span,
            },
        ) {
            Ok(event) => {
                let id = event.identifier.clone();
                if let Some(alarms) = &params.alarms {
                    apply_alarms_event(&manager, &id, alarms);
                }
                if let Some(recurrence) = &params.recurrence {
                    if recurrence.frequency.is_empty() {
                        let _ = manager.remove_event_recurrence_rules(&id);
                    } else if let Ok(rule) = parse_recurrence_param(recurrence) {
                        let _ = manager.set_event_recurrence_rule(&id, &rule);
                    }
                }
                let updated = manager.get_event(&id).unwrap_or(event);
                Ok(Json(EventOutput::from_item(&updated, &manager)))
            }
            Err(e) => Err(mcp_err(&e)),
        }
    }

    #[cfg(feature = "location")]
    #[tool(
        description = "Get the user's current location (latitude, longitude). Requires location permission."
    )]
    async fn get_current_location(&self) -> Result<Json<CoordinateOutput>, McpError> {
        let manager = crate::location::LocationManager::new();
        match manager.get_current_location(std::time::Duration::from_secs(10)) {
            Ok(coord) => Ok(Json(CoordinateOutput {
                latitude: coord.latitude,
                longitude: coord.longitude,
            })),
            Err(e) => Err(McpError::internal_error(e.to_string(), None)),
        }
    }
    // ========================================================================
    // Search Tools
    // ========================================================================

    #[tool(
        description = "Search reminders or events by text in title or notes (case-insensitive). Specify item_type to filter, or omit to search both."
    )]
    async fn search(
        &self,
        Parameters(params): Parameters<SearchRequest>,
    ) -> Result<Json<SearchResponse>, McpError> {
        let query = params.query.to_lowercase();

        let search_reminders = matches!(params.item_type, None | Some(ItemType::Reminder));
        let search_events = matches!(params.item_type, None | Some(ItemType::Event));

        let reminders = if search_reminders {
            let manager = RemindersManager::new();
            let items = if params.include_completed {
                manager.fetch_all_reminders()
            } else {
                manager.fetch_incomplete_reminders()
            };
            items.ok().map(|items| {
                let filtered: Vec<_> = items
                    .into_iter()
                    .filter(|r| {
                        r.title.to_lowercase().contains(&query)
                            || r.notes
                                .as_deref()
                                .is_some_and(|n| n.to_lowercase().contains(&query))
                    })
                    .map(|r| ReminderOutput::from_item_summary(&r))
                    .collect();
                ListResponse {
                    count: filtered.len(),
                    items: filtered,
                }
            })
        } else {
            None
        };

        let events = if search_events {
            let manager = EventsManager::new();
            let start = Local::now();
            let end = start + Duration::days(params.days);
            manager.fetch_events(start, end, None).ok().map(|items| {
                let filtered: Vec<_> = items
                    .into_iter()
                    .filter(|e| {
                        e.title.to_lowercase().contains(&query)
                            || e.notes
                                .as_deref()
                                .is_some_and(|n| n.to_lowercase().contains(&query))
                    })
                    .map(|e| EventOutput::from_item_summary(&e))
                    .collect();
                ListResponse {
                    count: filtered.len(),
                    items: filtered,
                }
            })
        } else {
            None
        };

        Ok(Json(SearchResponse {
            query: params.query,
            reminders,
            events,
        }))
    }

    // ========================================================================
    // Batch Operations
    // ========================================================================

    #[tool(description = "Delete multiple reminders or events at once.")]
    async fn batch_delete(
        &self,
        Parameters(params): Parameters<BatchDeleteRequest>,
    ) -> Result<Json<BatchResponse>, McpError> {
        let mut succeeded = 0usize;
        let mut errors = Vec::new();

        match params.item_type {
            ItemType::Reminder => {
                let manager = RemindersManager::new();
                for id in &params.item_ids {
                    match manager.delete_reminder(id) {
                        Ok(_) => succeeded += 1,
                        Err(e) => errors.push(format!("{id}: {e}")),
                    }
                }
            }
            ItemType::Event => {
                let manager = EventsManager::new();
                for id in &params.item_ids {
                    match manager.delete_event(id, params.affect_future) {
                        Ok(_) => succeeded += 1,
                        Err(e) => errors.push(format!("{id}: {e}")),
                    }
                }
            }
        }

        let err_items: Vec<_> = errors
            .into_iter()
            .map(|e| {
                let (id, msg) = e.split_once(": ").unwrap_or(("unknown", &e));
                BatchItemError {
                    item_id: id.to_string(),
                    message: msg.to_string(),
                }
            })
            .collect();
        Ok(Json(BatchResponse {
            total: params.item_ids.len(),
            succeeded,
            failed: err_items.len(),
            errors: err_items,
        }))
    }

    #[tool(description = "Move multiple reminders to a different list at once.")]
    async fn batch_move(
        &self,
        Parameters(params): Parameters<BatchMoveRequest>,
    ) -> Result<Json<BatchResponse>, McpError> {
        let manager = RemindersManager::new();
        let mut succeeded = 0usize;
        let mut errors = Vec::new();

        for id in &params.reminder_ids {
            match manager.update_reminder(
                id,
                &crate::ReminderPatch {
                    calendar_title: Some(&params.destination_list_name),
                    ..Default::default()
                },
            ) {
                Ok(_) => succeeded += 1,
                Err(e) => errors.push(format!("{id}: {e}")),
            }
        }

        let err_items: Vec<_> = errors
            .into_iter()
            .map(|e| {
                let (id, msg) = e.split_once(": ").unwrap_or(("unknown", &e));
                BatchItemError {
                    item_id: id.to_string(),
                    message: msg.to_string(),
                }
            })
            .collect();
        Ok(Json(BatchResponse {
            total: params.reminder_ids.len(),
            succeeded,
            failed: err_items.len(),
            errors: err_items,
        }))
    }

    #[tool(description = "Update multiple reminders or events at once.")]
    async fn batch_update(
        &self,
        Parameters(params): Parameters<BatchUpdateRequest>,
    ) -> Result<Json<BatchResponse>, McpError> {
        let mut succeeded = 0usize;
        let mut errors = Vec::new();

        match params.item_type {
            ItemType::Reminder => {
                let manager = RemindersManager::new();
                for item in &params.updates {
                    let priority = item.priority.as_ref().map(Priority::to_usize);
                    let due_date = match &item.due_date {
                        Some(s) if s.is_empty() => Some(None),
                        Some(s) => match parse_datetime_or_time(s) {
                            Ok(dt) => Some(Some(dt)),
                            Err(e) => {
                                errors.push(format!("{}: {e}", item.item_id));
                                continue;
                            }
                        },
                        None => None,
                    };
                    match manager.update_reminder(
                        &item.item_id,
                        &crate::ReminderPatch {
                            title: item.title.as_deref(),
                            notes: item.notes.as_deref(),
                            completed: item.completed,
                            priority,
                            due_date,
                            ..Default::default()
                        },
                    ) {
                        Ok(_) => succeeded += 1,
                        Err(e) => errors.push(format!("{}: {e}", item.item_id)),
                    }
                }
            }
            ItemType::Event => {
                let manager = EventsManager::new();
                for item in &params.updates {
                    match manager.update_event(
                        &item.item_id,
                        &crate::EventPatch {
                            title: item.title.as_deref(),
                            notes: item
                                .notes
                                .as_ref()
                                .map(|s| if s.is_empty() { None } else { Some(s.as_str()) }),
                            ..Default::default()
                        },
                    ) {
                        Ok(_) => succeeded += 1,
                        Err(e) => errors.push(format!("{}: {e}", item.item_id)),
                    }
                }
            }
        }

        let total = params.updates.len();
        let err_items: Vec<_> = errors
            .into_iter()
            .map(|e| {
                let (id, msg) = e.split_once(": ").unwrap_or(("unknown", &e));
                BatchItemError {
                    item_id: id.to_string(),
                    message: msg.to_string(),
                }
            })
            .collect();
        Ok(Json(BatchResponse {
            total,
            succeeded,
            failed: err_items.len(),
            errors: err_items,
        }))
    }
}

/// Parse a RecurrenceParam into a RecurrenceRule.
fn parse_recurrence_param(
    params: &RecurrenceParam,
) -> std::result::Result<crate::RecurrenceRule, String> {
    let frequency = match params.frequency.as_str() {
        "daily" => crate::RecurrenceFrequency::Daily,
        "weekly" => crate::RecurrenceFrequency::Weekly,
        "monthly" => crate::RecurrenceFrequency::Monthly,
        "yearly" => crate::RecurrenceFrequency::Yearly,
        other => {
            return Err(format!(
                "Invalid frequency: '{}'. Use daily, weekly, monthly, or yearly.",
                other
            ));
        }
    };

    let end = if let Some(count) = params.end_after_count {
        crate::RecurrenceEndCondition::AfterCount(count)
    } else if let Some(date_str) = &params.end_date {
        let dt = parse_datetime(date_str)?;
        crate::RecurrenceEndCondition::OnDate(dt)
    } else {
        crate::RecurrenceEndCondition::Never
    };

    // Range validation at boundary. Apple raises opaque NSExceptions for
    // out-of-range values, so we catch them here with friendlier messages.
    fn check_range(
        name: &str,
        vals: &Option<Vec<i32>>,
        valid: impl Fn(i32) -> bool,
    ) -> Result<(), String> {
        if let Some(vs) = vals {
            for v in vs {
                if !valid(*v) {
                    return Err(format!("{name}: value {v} out of range"));
                }
            }
        }
        Ok(())
    }
    check_range("days_of_month", &params.days_of_month, |v| {
        (-31..=31).contains(&v) && v != 0
    })?;
    check_range("months_of_year", &params.months_of_year, |v| {
        (1..=12).contains(&v)
    })?;
    check_range("weeks_of_year", &params.weeks_of_year, |v| {
        (-53..=53).contains(&v) && v != 0
    })?;
    check_range("days_of_year", &params.days_of_year, |v| {
        (-366..=366).contains(&v) && v != 0
    })?;
    check_range("set_positions", &params.set_positions, |v| {
        (-366..=366).contains(&v) && v != 0
    })?;

    Ok(crate::RecurrenceRule {
        frequency,
        interval: params.interval,
        end,
        days_of_week: params.days_of_week.clone(),
        days_of_month: params.days_of_month.clone(),
        months_of_year: params.months_of_year.clone(),
        weeks_of_year: params.weeks_of_year.clone(),
        days_of_year: params.days_of_year.clone(),
        set_positions: params.set_positions.clone(),
    })
}

/// Parse a date/time string, defaulting to today if only time is given.
fn parse_datetime_or_time(s: &str) -> Result<DateTime<Local>, String> {
    // Try full datetime or date first
    if let Ok(dt) = parse_datetime(s) {
        return Ok(dt);
    }
    // Try time-only: "HH:MM" → use today's date
    if let Ok(time) = chrono::NaiveTime::parse_from_str(s, "%H:%M") {
        let today = Local::now().date_naive();
        let dt = today.and_time(time);
        return Local
            .from_local_datetime(&dt)
            .single()
            .ok_or_else(|| "Invalid local datetime".to_string());
    }
    Err(
        "Invalid date format. Use 'YYYY-MM-DD', 'YYYY-MM-DD HH:MM', or 'HH:MM' (uses today)"
            .to_string(),
    )
}

/// Apply alarms to a reminder, clearing existing ones first.
fn apply_alarms_reminder(manager: &RemindersManager, id: &str, alarms: &[AlarmParam]) {
    // Clear existing alarms
    if let Ok(existing) = manager.get_alarms(id) {
        for i in (0..existing.len()).rev() {
            let _ = manager.remove_alarm(id, i);
        }
    }
    // Add new alarms
    for param in alarms {
        let alarm = alarm_param_to_info(param);
        let _ = manager.add_alarm(id, &alarm);
    }
}

/// Apply alarms to an event, clearing existing ones first.
fn apply_alarms_event(manager: &EventsManager, id: &str, alarms: &[AlarmParam]) {
    if let Ok(existing) = manager.get_event_alarms(id) {
        for i in (0..existing.len()).rev() {
            let _ = manager.remove_event_alarm(id, i);
        }
    }
    for param in alarms {
        let alarm = alarm_param_to_info(param);
        let _ = manager.add_event_alarm(id, &alarm);
    }
}

/// Convert an AlarmParam to an AlarmInfo.
fn alarm_param_to_info(param: &AlarmParam) -> crate::AlarmInfo {
    let proximity = match param.proximity.as_deref() {
        Some("enter") => crate::AlarmProximity::Enter,
        Some("leave") => crate::AlarmProximity::Leave,
        _ => crate::AlarmProximity::None,
    };
    let location = if let (Some(title), Some(lat), Some(lng)) =
        (&param.location_title, param.latitude, param.longitude)
    {
        Some(crate::StructuredLocation {
            title: title.clone(),
            latitude: lat,
            longitude: lng,
            radius: param.radius.unwrap_or(100.0),
        })
    } else {
        None
    };
    crate::AlarmInfo {
        relative_offset: param.relative_offset,
        proximity,
        location,
        email_address: param.email_address.clone(),
        sound_name: param.sound_name.clone(),
        url: param.url.clone(),
        ..Default::default()
    }
}

// ============================================================================
// Prompts
// ============================================================================

#[prompt_router]
impl EventKitServer {
    /// List all incomplete (not yet finished) reminders, optionally filtered by list name.
    #[prompt(
        name = "incomplete_reminders",
        description = "List all incomplete reminders"
    )]
    async fn incomplete_reminders(
        &self,
        Parameters(args): Parameters<ListRemindersPromptArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let manager = RemindersManager::new();
        let reminders = manager.fetch_incomplete_reminders().map_err(|e| {
            McpError::internal_error(format!("Failed to list reminders: {e}"), None)
        })?;

        // Filter by list name if provided
        let reminders: Vec<_> = if let Some(ref name) = args.list_name {
            reminders
                .into_iter()
                .filter(|r| r.calendar_title.as_deref() == Some(name.as_str()))
                .collect()
        } else {
            reminders
        };

        let mut output = String::new();
        for r in &reminders {
            output.push_str(&format!(
                "- [{}] {} (id: {}){}{}\n",
                if r.completed { "x" } else { " " },
                r.title,
                r.identifier,
                r.due_date
                    .map(|d| format!(", due: {}", d.format("%Y-%m-%d %H:%M")))
                    .unwrap_or_default(),
                r.calendar_title
                    .as_ref()
                    .map(|l| format!(", list: {l}"))
                    .unwrap_or_default(),
            ));
        }

        if output.is_empty() {
            output = "No incomplete reminders found.".to_string();
        }

        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Here are the current incomplete reminders:\n\n{output}\n\nPlease help me manage these reminders."
            ),
        )])
        .with_description("Incomplete reminders"))
    }

    /// List all reminder lists (calendars) available in macOS Reminders.
    #[prompt(
        name = "reminder_lists",
        description = "List all reminder lists available in Reminders"
    )]
    async fn reminder_lists_prompt(&self) -> Result<GetPromptResult, McpError> {
        let manager = RemindersManager::new();
        let lists = manager.list_calendars().map_err(|e| {
            McpError::internal_error(format!("Failed to list calendars: {e}"), None)
        })?;

        let mut output = String::new();
        for list in &lists {
            output.push_str(&format!("- {} (id: {})\n", list.title, list.identifier));
        }

        if output.is_empty() {
            output = "No reminder lists found.".to_string();
        }

        Ok(GetPromptResult::new(vec![PromptMessage::new_text(
            Role::User,
            format!(
                "Here are the available reminder lists:\n\n{output}\n\nWhich list would you like to work with?"
            ),
        )])
        .with_description("Available reminder lists"))
    }

    /// Move a reminder to a different reminder list.
    #[prompt(
        name = "move_reminder",
        description = "Move a reminder to a different list"
    )]
    async fn move_reminder_prompt(
        &self,
        Parameters(args): Parameters<MoveReminderPromptArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let manager = RemindersManager::new();

        // Find the destination calendar
        let lists = manager.list_calendars().map_err(|e| {
            McpError::internal_error(format!("Failed to list calendars: {e}"), None)
        })?;

        let dest = lists.iter().find(|l| {
            l.title
                .to_lowercase()
                .contains(&args.destination_list.to_lowercase())
        });

        match dest {
            Some(dest_list) => {
                match manager.update_reminder(
                    &args.reminder_id,
                    &crate::ReminderPatch {
                        calendar_title: Some(&dest_list.title),
                        ..Default::default()
                    },
                ) {
                    Ok(updated) => Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                        Role::User,
                        format!(
                            "Moved reminder \"{}\" to list \"{}\".",
                            updated.title, dest_list.title
                        ),
                    )])
                    .with_description("Reminder moved")),
                    Err(e) => Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                        Role::User,
                        format!("Failed to move reminder: {e}"),
                    )])
                    .with_description("Move failed")),
                }
            }
            None => {
                let available: Vec<&str> = lists.iter().map(|l| l.title.as_str()).collect();
                Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                    Role::User,
                    format!(
                        "Could not find reminder list \"{}\". Available lists: {}",
                        args.destination_list,
                        available.join(", ")
                    ),
                )])
                .with_description("List not found"))
            }
        }
    }

    /// Create a new reminder with optional notes, priority, due date, and list.
    #[prompt(
        name = "create_detailed_reminder",
        description = "Create a reminder with detailed context like notes, priority, and due date"
    )]
    async fn create_detailed_reminder_prompt(
        &self,
        Parameters(args): Parameters<CreateReminderPromptArgs>,
    ) -> Result<GetPromptResult, McpError> {
        let manager = RemindersManager::new();

        let due = args
            .due_date
            .as_deref()
            .map(parse_datetime)
            .transpose()
            .map_err(|e| McpError::internal_error(format!("Invalid due date: {e}"), None))?;

        match manager.create_reminder(&crate::ReminderDraft {
            title: &args.title,
            notes: args.notes.as_deref(),
            calendar_title: args.list_name.as_deref(),
            priority: args.priority.map(|p| p as usize),
            due_date: due,
            ..Default::default()
        }) {
            Ok(reminder) => {
                let mut details = format!("Created reminder: \"{}\"", reminder.title);
                if let Some(notes) = &reminder.notes {
                    details.push_str(&format!("\nNotes: {notes}"));
                }
                if reminder.priority > 0 {
                    details.push_str(&format!("\nPriority: {}", reminder.priority));
                }
                if let Some(due) = &reminder.due_date {
                    details.push_str(&format!("\nDue: {}", due.format("%Y-%m-%d %H:%M")));
                }
                if let Some(list) = &reminder.calendar_title {
                    details.push_str(&format!("\nList: {list}"));
                }

                Ok(
                    GetPromptResult::new(vec![PromptMessage::new_text(Role::User, details)])
                        .with_description("Reminder created"),
                )
            }
            Err(e) => Ok(GetPromptResult::new(vec![PromptMessage::new_text(
                Role::User,
                format!("Failed to create reminder: {e}"),
            )])
            .with_description("Creation failed")),
        }
    }
}

// Implement the server handler
#[tool_handler]
#[prompt_handler]
impl rmcp::ServerHandler for EventKitServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_prompts()
                .build(),
        )
        .with_instructions(
            "This MCP server provides access to macOS Calendar events and Reminders. \
             Use the available tools to list, create, update, and delete calendar events \
             and reminders. Authorization is handled automatically on first use.",
        )
    }
}

/// Serve the EventKit MCP server on any async read/write transport.
///
/// Used by the in-process gateway (via `DuplexStream`) and for testing.
/// The standalone binary uses [`run_mcp_server`] which wraps this with stdio.
pub async fn serve_on<T>(transport: T) -> anyhow::Result<()>
where
    T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let server = EventKitServer::new();
    let service = server.serve(transport).await?;
    service.waiting().await?;
    Ok(())
}

/// Run the EventKit MCP server on stdio transport.
///
/// This initializes logging to stderr (MCP uses stdout/stdin for protocol)
/// and starts the MCP server. Used by the standalone binary (`eventkit --mcp`).
pub async fn run_mcp_server() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .init();

    let server = EventKitServer::new();
    let service = server.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

// ============================================================================
// Dump helpers — serialize objects to JSON for CLI debugging
// ============================================================================

/// Dump a single reminder as pretty JSON (with alarms and recurrence rules).
pub fn dump_reminder(id: &str) -> Result<String, crate::EventKitError> {
    let manager = RemindersManager::new();
    let r = manager.get_reminder(id)?;
    let output = ReminderOutput::from_item(&r, &manager);
    Ok(serde_json::to_string_pretty(&output).unwrap())
}

/// Dump every Objective-C `@property` on the reminder, its calendar, and that
/// calendar's source — using runtime reflection. Use this to discover native
/// fields not yet surfaced by [`crate::ReminderItem`].
///
/// `read_values` controls whether property values are read via KVC. Schema-
/// only (`false`) is always safe; reading values may surface NSExceptions and
/// — for a small denylisted set — could abort the process via C asserts.
pub fn dump_reminder_raw(id: &str, read_values: bool) -> Result<String, crate::EventKitError> {
    let manager = RemindersManager::new();
    manager.dump_reminder_raw(id, read_values)
}

/// Probe a curated list of suspected-private selectors on a reminder
/// (`richLink`, `tags`, `structuredData`, etc.). Read-only, exception-safe.
pub fn dump_reminder_private(id: &str) -> Result<String, crate::EventKitError> {
    let manager = RemindersManager::new();
    manager.dump_reminder_private(id)
}

/// Probe suspected-private selectors on an EVENT. Read-only, exception-safe.
///
/// Same probe list as `dump_reminder_private`, so a rich-notes finding on
/// either EKReminder or EKEvent shows up in both reports. `notes` is included
/// as a baseline — compare its reported CLASS against any attributed-notes hit.
pub fn dump_event_private(id: &str) -> Result<String, crate::EventKitError> {
    let manager = EventsManager::new();
    manager.dump_event_private(id)
}

/// Dump all reminders as pretty JSON (summary mode — no alarm/recurrence fetch).
pub fn dump_reminders(list_name: Option<&str>) -> Result<String, crate::EventKitError> {
    let manager = RemindersManager::new();
    let items = manager.fetch_all_reminders()?;
    let filtered: Vec<_> = if let Some(name) = list_name {
        items
            .into_iter()
            .filter(|r| r.calendar_title.as_deref() == Some(name))
            .collect()
    } else {
        items
    };
    let output: Vec<_> = filtered
        .iter()
        .map(ReminderOutput::from_item_summary)
        .collect();
    Ok(serde_json::to_string_pretty(&output).unwrap())
}

/// Dump a single event as pretty JSON (with alarms, recurrence, attendees).
pub fn dump_event(id: &str) -> Result<String, crate::EventKitError> {
    let manager = EventsManager::new();
    let e = manager.get_event(id)?;
    let output = EventOutput::from_item(&e, &manager);
    Ok(serde_json::to_string_pretty(&output).unwrap())
}

/// Dump upcoming events as pretty JSON.
pub fn dump_events(days: i64) -> Result<String, crate::EventKitError> {
    let manager = EventsManager::new();
    let items = manager.fetch_upcoming_events(days)?;
    let output: Vec<_> = items.iter().map(EventOutput::from_item_summary).collect();
    Ok(serde_json::to_string_pretty(&output).unwrap())
}

/// Dump all reminder lists as pretty JSON.
pub fn dump_reminder_lists() -> Result<String, crate::EventKitError> {
    let manager = RemindersManager::new();
    let lists = manager.list_calendars()?;
    let output: Vec<_> = lists.iter().map(CalendarOutput::from_info).collect();
    Ok(serde_json::to_string_pretty(&output).unwrap())
}

/// Dump all event calendars as pretty JSON.
pub fn dump_calendars() -> Result<String, crate::EventKitError> {
    let manager = EventsManager::new();
    let cals = manager.list_calendars()?;
    let output: Vec<_> = cals.iter().map(CalendarOutput::from_info).collect();
    Ok(serde_json::to_string_pretty(&output).unwrap())
}

/// Dump all sources as pretty JSON.
pub fn dump_sources() -> Result<String, crate::EventKitError> {
    let manager = RemindersManager::new();
    let sources = manager.list_sources()?;
    let output: Vec<_> = sources.iter().map(SourceOutput::from_info).collect();
    Ok(serde_json::to_string_pretty(&output).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EventKitError;

    #[test]
    fn parse_availability_accepts_every_variant() {
        for (s, expected) in [
            ("busy", crate::EventAvailability::Busy),
            ("free", crate::EventAvailability::Free),
            ("tentative", crate::EventAvailability::Tentative),
            ("unavailable", crate::EventAvailability::Unavailable),
            ("not_supported", crate::EventAvailability::NotSupported),
        ] {
            assert_eq!(parse_availability(s).unwrap(), expected);
        }
        assert!(parse_availability("BUSY").is_err()); // case-sensitive
        assert!(parse_availability("anything else").is_err());
    }

    #[test]
    fn parse_span_defaults_to_this() {
        assert_eq!(parse_span(None).unwrap(), crate::EventSpan::This);
        assert_eq!(parse_span(Some("this")).unwrap(), crate::EventSpan::This);
        assert_eq!(
            parse_span(Some("future")).unwrap(),
            crate::EventSpan::Future
        );
        assert!(parse_span(Some("bogus")).is_err());
    }

    #[test]
    fn create_event_request_deserializes_new_fields() {
        // Catches accidental rename of the new fields on the input schema.
        let json = serde_json::json!({
            "title": "Plan",
            "start": "2026-06-01 14:00",
            "all_day": false,
            "availability": "tentative",
            "structured_location": {
                "title": "Office",
                "latitude": 37.78,
                "longitude": -122.42,
                "radius_meters": 100.0
            }
        });
        let req: CreateEventRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.availability.as_deref(), Some("tentative"));
        assert_eq!(req.structured_location.unwrap().title, "Office");
    }

    #[test]
    fn update_event_request_deserializes_span_and_new_fields() {
        let json = serde_json::json!({
            "event_id": "ABC",
            "all_day": true,
            "calendar_name": "Work",
            "availability": "free",
            "span": "future",
        });
        let req: UpdateEventRequest = serde_json::from_value(json).unwrap();
        assert_eq!(req.all_day, Some(true));
        assert_eq!(req.calendar_name.as_deref(), Some("Work"));
        assert_eq!(req.availability.as_deref(), Some("free"));
        assert_eq!(req.span.as_deref(), Some("future"));
    }

    #[test]
    fn auth_status_str_covers_every_variant() {
        // Update this whenever AuthorizationStatus gains a variant — that's
        // the whole point: a new variant breaks compilation here, and the
        // MCP `auth_status` response stays in sync with the source enum.
        for s in [
            AuthorizationStatus::NotDetermined,
            AuthorizationStatus::Restricted,
            AuthorizationStatus::Denied,
            AuthorizationStatus::FullAccess,
            AuthorizationStatus::WriteOnly,
        ] {
            let str_form = auth_status_str(s);
            assert!(!str_form.is_empty(), "empty string for {s:?}");
            assert!(
                !str_form.contains(' '),
                "MCP wire format should be PascalCase, got {str_form:?} for {s:?}"
            );
        }
    }

    /// Only FULL access on BOTH entities means "nothing to fix".
    #[test]
    fn auth_remediation_absent_only_when_both_have_full_access() {
        assert!(
            auth_remediation(
                AuthorizationStatus::FullAccess,
                AuthorizationStatus::FullAccess
            )
            .is_none(),
            "full access on both needs no remediation"
        );
    }

    /// Regression guard for the `auth_status` half of the write-only defect:
    /// a write-only grant MUST still produce a remediation hint. Treating it
    /// as granted told the user everything was fine while every read came
    /// back silently empty.
    #[test]
    fn auth_remediation_present_for_write_only() {
        for (r, e) in [
            (
                AuthorizationStatus::WriteOnly,
                AuthorizationStatus::WriteOnly,
            ),
            (
                AuthorizationStatus::WriteOnly,
                AuthorizationStatus::FullAccess,
            ),
            (
                AuthorizationStatus::FullAccess,
                AuthorizationStatus::WriteOnly,
            ),
        ] {
            let hint = auth_remediation(r, e)
                .unwrap_or_else(|| panic!("write-only must produce a hint for ({r:?}, {e:?})"));
            assert!(
                hint.to_lowercase().contains("full"),
                "the hint must name FULL access, got: {hint}"
            );
        }
    }

    #[test]
    fn auth_remediation_picks_worst_status() {
        // Denied (3) > Restricted (2) > NotDetermined (1) > granted (0).
        // When reminders=NotDetermined and events=Denied, hint should reflect
        // the Denied state.
        let hint = auth_remediation(
            AuthorizationStatus::NotDetermined,
            AuthorizationStatus::Denied,
        )
        .expect("expected remediation");
        assert!(
            hint.contains("System Settings") || hint.contains("tccutil"),
            "Denied remediation should mention System Settings or tccutil; got: {hint}"
        );
    }

    #[test]
    fn auth_remediation_notdetermined_mentions_consent_dialog() {
        let hint = auth_remediation(
            AuthorizationStatus::NotDetermined,
            AuthorizationStatus::NotDetermined,
        )
        .expect("expected remediation");
        assert!(
            hint.contains("consent dialog"),
            "NotDetermined remediation should mention the consent dialog; got: {hint}"
        );
    }

    #[test]
    fn mcp_err_includes_remediation_hint_for_auth_variants() {
        // The plain error message is just "Authorization denied"; mcp_err
        // wraps it with actionable guidance for the agent. If this test
        // breaks, the agent loses its diagnostic hint.
        let err = mcp_err(&EventKitError::AuthorizationDenied);
        assert!(
            err.message.contains("System Settings") && err.message.contains("auth_status"),
            "AuthorizationDenied should mention System Settings and the auth_status tool; got: {}",
            err.message
        );

        let err = mcp_err(&EventKitError::AuthorizationNotDetermined);
        assert!(
            err.message.contains("Info.plist"),
            "AuthorizationNotDetermined should hint at Info.plist usage strings; got: {}",
            err.message
        );

        let err = mcp_err(&EventKitError::AuthorizationRestricted);
        assert!(
            err.message.contains("MDM") || err.message.contains("policy"),
            "AuthorizationRestricted should mention policy/MDM; got: {}",
            err.message
        );
    }

    #[test]
    fn mcp_err_passes_through_non_auth_errors() {
        let err = mcp_err(&EventKitError::ItemNotFound("xyz".into()));
        assert!(
            err.message.contains("xyz"),
            "non-auth errors should preserve the original message; got: {}",
            err.message
        );
    }
}
