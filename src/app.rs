use chrono::{DateTime, Duration, Local, NaiveDateTime, TimeZone};
use clap::{Parser, Subcommand};
use eventkit::{AuthorizationStatus, EventKitError, EventsManager, RemindersManager};

#[derive(Parser)]
#[command(name = "eventkit")]
#[command(author, version, about = "Manage macOS Calendar and Reminders from the command line", long_about = None)]
struct Cli {
    /// Run as an MCP (Model Context Protocol) server over stdio
    #[cfg(feature = "mcp")]
    #[arg(long)]
    mcp: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// Commands for managing reminders
    #[command(subcommand)]
    Reminders(RemindersCommands),

    /// Commands for managing calendar events
    #[command(subcommand)]
    Events(EventsCommands),

    /// Check authorization status
    Status {
        /// Check events status instead of reminders
        #[arg(short, long)]
        events: bool,
    },

    /// Dump raw objects as JSON for debugging
    #[cfg(feature = "mcp")]
    #[command(subcommand)]
    Dump(DumpCommands),
}

#[derive(Subcommand)]
#[allow(non_snake_case)]
enum RemindersCommands {
    /// Request authorization to access reminders
    Authorize,

    /// List all reminder lists (calendars)
    Lists,

    /// List reminders
    List {
        /// Filter by specific list(s)
        #[arg(short, long)]
        list: Option<Vec<String>>,

        /// Show only incomplete reminders
        #[arg(short, long)]
        incomplete: bool,

        /// Show completed reminders
        #[arg(short, long)]
        completed: bool,

        /// Show all details
        #[arg(short, long)]
        all: bool,

        /// Show all fields for debugging
        #[arg(long)]
        debug: bool,

        /// Filter incomplete reminders to those due at/after this timestamp
        /// (`YYYY-MM-DD` or `YYYY-MM-DD HH:MM`).
        #[arg(long = "due-after")]
        due_after: Option<String>,

        /// Filter incomplete reminders to those due before this timestamp.
        #[arg(long = "due-before")]
        due_before: Option<String>,

        /// Filter to completed reminders completed at/after this timestamp.
        /// Implies completed-only.
        #[arg(long = "completed-after")]
        completed_after: Option<String>,

        /// Filter to completed reminders completed before this timestamp.
        /// Implies completed-only.
        #[arg(long = "completed-before")]
        completed_before: Option<String>,
    },

    /// Create a new reminder
    Add {
        /// Title of the reminder
        title: String,

        /// Notes/description for the reminder
        #[arg(short, long)]
        notes: Option<String>,

        /// List to add the reminder to
        #[arg(short, long)]
        list: Option<String>,

        /// Priority (0=none, 1-4=high, 5=medium, 6-9=low)
        #[arg(short, long)]
        priority: Option<usize>,

        /// IANA timezone for the due date (e.g. America/Los_Angeles)
        #[arg(long = "due-tz")]
        due_tz: Option<String>,
    },

    /// Update an existing reminder
    Update {
        /// Identifier of the reminder to update
        id: String,

        /// New title
        #[arg(short, long)]
        title: Option<String>,

        /// New notes
        #[arg(short, long)]
        notes: Option<String>,

        /// Priority (0=none, 1-4=high, 5=medium, 6-9=low)
        #[arg(short, long)]
        priority: Option<usize>,

        /// Explicit completion timestamp. Setting marks the reminder
        /// complete; pass an empty string to clear (marks incomplete).
        /// Format: `YYYY-MM-DD` or `YYYY-MM-DD HH:MM`.
        #[arg(long = "completion-date")]
        completion_date: Option<String>,
    },

    /// Set or clear the IANA timezone applied specifically to the due date.
    SetDueTz {
        id: String,
        /// IANA zone name, e.g. America/Los_Angeles (use "" to clear)
        tz: String,
    },

    /// Diagnostic: set or clear `EKReminder.URL`. Reminders.app's UI typically
    /// ignores this field on iCloud-synced reminders (see project notes), but
    /// the setter still writes to EventKit for inspection / CalDAV consumers.
    #[command(name = "set-URL")]
    #[allow(non_camel_case_types)]
    SetURL {
        id: String,
        /// New URL (use "" to clear). Strictly validated per RFC 3986.
        #[allow(non_snake_case)]
        URL: String,
    },

    /// Attach a geofence ("remind me when I arrive/leave") to a reminder by
    /// adding a location-based alarm. Triggers the Location permission
    /// prompt the first time it's used.
    SetGeofence {
        id: String,
        /// Display title for the location ("Home", "Office", ...)
        #[arg(long = "loc-title")]
        title: String,
        #[arg(long)]
        lat: f64,
        #[arg(long)]
        lng: f64,
        /// Radius in meters.
        #[arg(long, default_value = "100")]
        radius: f64,
        /// "enter" or "leave".
        #[arg(long, default_value = "enter")]
        proximity: String,
    },

    /// Clear any geofence-bearing alarm from the reminder.
    ClearGeofence { id: String },

    /// Mark a reminder as complete
    Complete {
        /// Identifier of the reminder
        id: String,
    },

    /// Mark a reminder as incomplete
    Uncomplete {
        /// Identifier of the reminder
        id: String,
    },

    /// Delete a reminder
    Delete {
        /// Identifier of the reminder to delete
        id: String,

        /// Skip confirmation
        #[arg(short, long)]
        force: bool,
    },

    /// Show details of a specific reminder
    Show {
        /// Identifier of the reminder
        id: String,
    },
}

#[derive(Subcommand)]
enum EventsCommands {
    /// Request authorization to access calendar events
    Authorize,

    /// List all calendars
    Calendars,

    /// List events
    List {
        /// Show events for today only
        #[arg(short, long)]
        today: bool,

        /// Show events for the next N days (default: 7)
        #[arg(short, long, default_value = "7")]
        days: i64,

        /// Filter by specific calendar(s)
        #[arg(short, long)]
        calendar: Option<Vec<String>>,

        /// Show all details
        #[arg(short, long)]
        all: bool,
    },

    /// Create a new event
    Add {
        /// Title of the event
        title: String,

        /// Start date/time (format: YYYY-MM-DD HH:MM or YYYY-MM-DD for all-day)
        #[arg(short, long)]
        start: String,

        /// End date/time (format: YYYY-MM-DD HH:MM or YYYY-MM-DD for all-day)
        #[arg(short, long)]
        end: Option<String>,

        /// Duration in minutes (alternative to --end)
        #[arg(short, long, default_value = "60")]
        duration: i64,

        /// Notes/description
        #[arg(short, long)]
        notes: Option<String>,

        /// Location
        #[arg(short, long)]
        location: Option<String>,

        /// Calendar to add the event to
        #[arg(short, long)]
        calendar: Option<String>,

        /// Create as all-day event
        #[arg(long)]
        all_day: bool,

        /// URL to associate with the event
        #[arg(long)]
        url: Option<String>,

        /// Availability: busy (default), free, tentative, unavailable
        #[arg(long)]
        availability: Option<String>,
    },

    /// Update an existing event. Any field you don't supply is left as-is.
    /// Use `--span future` to propagate the edit to all later occurrences
    /// in a recurring series.
    Update {
        /// Identifier of the event to update
        id: String,

        /// New title
        #[arg(long)]
        title: Option<String>,

        /// New notes (use "" to clear)
        #[arg(long)]
        notes: Option<String>,

        /// New location (use "" to clear)
        #[arg(long)]
        location: Option<String>,

        /// New start (YYYY-MM-DD or YYYY-MM-DD HH:MM)
        #[arg(long)]
        start: Option<String>,

        /// New end (YYYY-MM-DD or YYYY-MM-DD HH:MM)
        #[arg(long)]
        end: Option<String>,

        /// Toggle all-day on / off
        #[arg(long)]
        all_day: Option<bool>,

        /// Move to another calendar by name
        #[arg(long)]
        calendar: Option<String>,

        /// New URL (use "" to clear)
        #[arg(long)]
        url: Option<String>,

        /// Availability: busy | free | tentative | unavailable
        #[arg(long)]
        availability: Option<String>,

        /// Edit scope for recurring events: "this" (default) or "future"
        #[arg(long, default_value = "this")]
        span: String,
    },

    /// Delete an event
    Delete {
        /// Identifier of the event to delete
        id: String,

        /// Skip confirmation
        #[arg(short, long)]
        force: bool,

        /// Edit scope for recurring events: "this" (default) or "future"
        #[arg(long, default_value = "this")]
        span: String,
    },

    /// Show details of a specific event
    Show {
        /// Identifier of the event
        id: String,
    },
}

#[cfg(feature = "mcp")]
#[derive(Subcommand)]
enum DumpCommands {
    /// Dump a single reminder with all fields, alarms, recurrence as JSON
    Reminder {
        /// Identifier of the reminder
        id: String,
    },
    /// Dump every native Objective-C `@property` on a reminder (and its
    /// calendar + source) using runtime reflection. Use this to discover
    /// EventKit fields that aren't yet surfaced by the curated `ReminderItem`.
    ReminderRaw {
        /// Identifier of the reminder
        id: String,

        /// Also read each property's current value via KVC. May surface
        /// NSExceptions for some keys; a small denylist skips keys known
        /// to abort the process via C asserts (e.g. `objectID`).
        #[arg(long)]
        values: bool,
    },
    /// Probe a curated list of suspected-private selectors on a reminder
    /// (`richLink`, `tags`, `structuredData`, …). Read-only — useful for
    /// finding where Reminders.app stores its UI rich-link URL and
    /// first-class tag entities that the public EventKit API doesn't surface.
    ReminderPrivate {
        /// Identifier of the reminder
        id: String,
    },
    /// Probe suspected-private selectors on an EVENT, reporting the ObjC
    /// CLASS of each value. Use this to settle whether a private rich-notes
    /// property exists: `notes` is probed as a baseline (expect
    /// `__NSCFString`), so anything answering with `NSAttributedString` or
    /// RTF-bearing `NSData` would mean rich text is reachable.
    EventPrivate {
        /// Identifier of the event
        id: String,
    },
    /// Dump all reminders in a list (or all lists) as JSON
    Reminders {
        /// Filter to a specific list name
        #[arg(short, long)]
        list: Option<String>,
    },
    /// Dump a single event with all fields, alarms, recurrence, attendees as JSON
    Event {
        /// Identifier of the event
        id: String,
    },
    /// Dump events for the next N days as JSON
    Events {
        /// Number of days (default: 7)
        #[arg(short, long, default_value = "7")]
        days: i64,
    },
    /// Dump all reminder lists as JSON
    ReminderLists,
    /// Dump all event calendars as JSON
    Calendars,
    /// Dump all sources as JSON
    Sources,
}

pub fn run() {
    let cli = Cli::parse();

    // Handle --mcp flag
    #[cfg(feature = "mcp")]
    if cli.mcp {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("Failed to create tokio runtime");
        if let Err(e) = rt.block_on(eventkit::mcp::run_mcp_server()) {
            eprintln!("MCP server error: {}", e);
            std::process::exit(1);
        }
        return;
    }

    let Some(command) = cli.command else {
        // No subcommand and no --mcp flag: print help
        use clap::CommandFactory;
        Cli::command().print_help().ok();
        println!();
        std::process::exit(1);
    };

    let result = match command {
        Commands::Status { events } => cmd_status(events),
        #[cfg(feature = "mcp")]
        Commands::Dump(cmd) => cmd_dump(cmd),
        Commands::Reminders(cmd) => match cmd {
            RemindersCommands::Authorize => cmd_reminders_authorize(),
            RemindersCommands::Lists => cmd_reminders_lists(),
            RemindersCommands::List {
                list,
                incomplete,
                completed,
                all,
                debug,
                due_after,
                due_before,
                completed_after,
                completed_before,
            } => cmd_reminders_list(
                list,
                incomplete,
                completed,
                all,
                debug,
                due_after.as_deref(),
                due_before.as_deref(),
                completed_after.as_deref(),
                completed_before.as_deref(),
            ),
            RemindersCommands::Add {
                title,
                notes,
                list,
                priority,
                due_tz,
            } => cmd_reminders_add(
                &title,
                notes.as_deref(),
                list.as_deref(),
                priority,
                due_tz.as_deref(),
            ),
            RemindersCommands::Update {
                id,
                title,
                notes,
                priority,
                completion_date,
            } => cmd_reminders_update(
                &id,
                title.as_deref(),
                notes.as_deref(),
                priority,
                completion_date.as_deref(),
            ),
            RemindersCommands::Complete { id } => cmd_reminders_complete(&id),
            RemindersCommands::Uncomplete { id } => cmd_reminders_uncomplete(&id),
            RemindersCommands::Delete { id, force } => cmd_reminders_delete(&id, force),
            RemindersCommands::Show { id } => cmd_reminders_show(&id),
            RemindersCommands::SetDueTz { id, tz } => cmd_reminders_set_due_tz(&id, &tz),
            RemindersCommands::SetURL { id, URL } => cmd_reminders_set_URL(&id, &URL),
            RemindersCommands::SetGeofence {
                id,
                title,
                lat,
                lng,
                radius,
                proximity,
            } => cmd_reminders_set_geofence(&id, &title, lat, lng, radius, &proximity),
            RemindersCommands::ClearGeofence { id } => cmd_reminders_clear_geofence(&id),
        },
        Commands::Events(cmd) => match cmd {
            EventsCommands::Authorize => cmd_events_authorize(),
            EventsCommands::Calendars => cmd_events_calendars(),
            EventsCommands::List {
                today,
                days,
                calendar,
                all,
            } => cmd_events_list(today, days, calendar, all),
            EventsCommands::Add {
                title,
                start,
                end,
                duration,
                notes,
                location,
                calendar,
                all_day,
                url,
                availability,
            } => cmd_events_add(
                &title,
                &start,
                end.as_deref(),
                duration,
                notes.as_deref(),
                location.as_deref(),
                calendar.as_deref(),
                all_day,
                url.as_deref(),
                availability.as_deref(),
            ),
            EventsCommands::Update {
                id,
                title,
                notes,
                location,
                start,
                end,
                all_day,
                calendar,
                url,
                availability,
                span,
            } => cmd_events_update(
                &id,
                title.as_deref(),
                notes.as_deref(),
                location.as_deref(),
                start.as_deref(),
                end.as_deref(),
                all_day,
                calendar.as_deref(),
                url.as_deref(),
                availability.as_deref(),
                &span,
            ),
            EventsCommands::Delete { id, force, span } => cmd_events_delete(&id, force, &span),
            EventsCommands::Show { id } => cmd_events_show(&id),
        },
    };

    if let Err(e) = result {
        eprintln!("Error: {}", e);
        std::process::exit(1);
    }
}

// ============================================================================
// Status command
// ============================================================================

fn cmd_status(events: bool) -> Result<(), EventKitError> {
    let (kind, status) = if events {
        ("Calendar Events", EventsManager::authorization_status())
    } else {
        ("Reminders", RemindersManager::authorization_status())
    };

    println!("{} Authorization Status: {}", kind, status);

    match status {
        AuthorizationStatus::NotDetermined => {
            println!(
                "\nUse 'eventkit {} authorize' to request access.",
                if events { "events" } else { "reminders" }
            );
        }
        AuthorizationStatus::Denied => {
            println!("\nAccess was denied. Please enable access in:");
            println!(
                "System Settings > Privacy & Security > {}",
                if events { "Calendars" } else { "Reminders" }
            );
        }
        AuthorizationStatus::Restricted => {
            println!("\nAccess is restricted by system policy.");
        }
        AuthorizationStatus::FullAccess => {
            println!("\nFull access granted.");
        }
        AuthorizationStatus::WriteOnly => {
            println!("\nWrite-only access granted.");
        }
    }

    Ok(())
}

// ============================================================================
// Reminders commands
// ============================================================================

fn cmd_reminders_authorize() -> Result<(), EventKitError> {
    let manager = RemindersManager::new();

    println!("Requesting access to Reminders...");

    match manager.request_access() {
        Ok(true) => {
            println!("✓ Access granted!");
            Ok(())
        }
        Ok(false) => {
            println!("✗ Access denied.");
            println!("\nTo grant access, go to:");
            println!("System Settings > Privacy & Security > Reminders");
            Err(EventKitError::AuthorizationDenied)
        }
        Err(e) => {
            println!("✗ Failed to request access: {}", e);
            Err(e)
        }
    }
}

fn cmd_reminders_lists() -> Result<(), EventKitError> {
    let manager = RemindersManager::new();
    let calendars = manager.list_calendars()?;

    if calendars.is_empty() {
        println!("No reminder lists found.");
        return Ok(());
    }

    println!("Reminder Lists:\n");

    for cal in calendars {
        let source = cal.source.as_deref().unwrap_or("Unknown");
        let modifiable = if cal.allows_modifications {
            ""
        } else {
            " (read-only)"
        };
        println!("  • {} [{}]{}", cal.title, source, modifiable);
        println!("    ID: {}", cal.identifier);
    }

    if let Ok(default) = manager.default_calendar() {
        println!("\nDefault list: {}", default.title);
    }

    Ok(())
}

#[allow(clippy::needless_pass_by_value, clippy::too_many_arguments)]
fn cmd_reminders_list(
    list_filter: Option<Vec<String>>,
    incomplete: bool,
    show_completed: bool,
    show_all: bool,
    debug: bool,
    due_after: Option<&str>,
    due_before: Option<&str>,
    completed_after: Option<&str>,
    completed_before: Option<&str>,
) -> Result<(), EventKitError> {
    let manager = RemindersManager::new();

    fn parse_cli_date(
        label: &str,
        s: Option<&str>,
    ) -> Result<Option<DateTime<Local>>, EventKitError> {
        match s {
            None => Ok(None),
            Some(raw) => {
                // Reuse chrono parsing inline since cmd_status's helper isn't exported here.
                if let Ok(naive) = NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M") {
                    return Local
                        .from_local_datetime(&naive)
                        .single()
                        .ok_or_else(|| EventKitError::SaveFailed(format!("Invalid {label}: {raw}")))
                        .map(Some);
                }
                if let Ok(date) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
                    let naive = date.and_hms_opt(0, 0, 0).unwrap();
                    return Local
                        .from_local_datetime(&naive)
                        .single()
                        .ok_or_else(|| EventKitError::SaveFailed(format!("Invalid {label}: {raw}")))
                        .map(Some);
                }
                Err(EventKitError::SaveFailed(format!(
                    "Invalid {label}: '{raw}'. Use YYYY-MM-DD or YYYY-MM-DD HH:MM."
                )))
            }
        }
    }
    let due_after = parse_cli_date("--due-after", due_after)?;
    let due_before = parse_cli_date("--due-before", due_before)?;
    let completed_after = parse_cli_date("--completed-after", completed_after)?;
    let completed_before = parse_cli_date("--completed-before", completed_before)?;

    let list_owned = list_filter.clone();
    let list_refs_storage: Option<Vec<&str>> = list_owned
        .as_ref()
        .map(|l| l.iter().map(std::string::String::as_str).collect());
    let list_refs: Option<&[&str]> = list_refs_storage.as_deref();

    let reminders = if completed_after.is_some() || completed_before.is_some() {
        manager.fetch_completed_reminders_in_range(completed_after, completed_before, list_refs)?
    } else if due_after.is_some() || due_before.is_some() {
        manager.fetch_incomplete_reminders_in_due_range(due_after, due_before, list_refs)?
    } else if incomplete {
        manager.fetch_incomplete_reminders_in_due_range(None, None, list_refs)?
    } else if let Some(refs) = list_refs {
        manager.fetch_reminders(Some(refs))?
    } else {
        manager.fetch_all_reminders()?
    };

    let reminders: Vec<_> = if !incomplete && !show_completed && !show_all {
        reminders.into_iter().filter(|r| !r.completed).collect()
    } else if show_completed && !show_all {
        reminders.into_iter().filter(|r| r.completed).collect()
    } else {
        reminders
    };

    if reminders.is_empty() {
        println!("No reminders found.");
        return Ok(());
    }

    println!("Reminders ({}):\n", reminders.len());

    for reminder in reminders {
        let status = if reminder.completed { "✓" } else { "○" };
        let priority_str = match reminder.priority {
            0 => String::new(),
            1..=4 => " !!!".to_string(),
            5 => " !!".to_string(),
            _ => " !".to_string(),
        };

        println!("  {} {}{}", status, reminder.title, priority_str);

        if show_all {
            if let Some(ref notes) = reminder.notes {
                let truncated: String = notes.chars().take(60).collect();
                let suffix = if notes.len() > 60 { "..." } else { "" };
                println!("      Notes: {}{}", truncated, suffix);
            }
            if let Some(ref cal) = reminder.calendar_title {
                println!("      List: {}", cal);
            }
            println!("      ID: {}", reminder.identifier);
        }

        if debug {
            // Print all available fields for debugging
            println!("      Completed: {}", reminder.completed);
            println!("      Priority: {}", reminder.priority);

            if let Some(due_date) = reminder.due_date {
                println!("      Due Date: {}", due_date.format("%Y-%m-%d %H:%M:%S"));
            } else {
                println!("      Due Date: None");
            }

            if let Some(start_date) = reminder.start_date {
                println!(
                    "      Start Date: {}",
                    start_date.format("%Y-%m-%d %H:%M:%S")
                );
            } else {
                println!("      Start Date: None");
            }

            if let Some(completion_date) = reminder.completion_date {
                println!(
                    "      Completion Date: {}",
                    completion_date.format("%Y-%m-%d %H:%M:%S")
                );
            } else {
                println!("      Completion Date: None");
            }

            // Additional inherited fields from EKCalendarItem parent class
            if let Some(ref notes) = reminder.notes {
                println!("      Notes: {}", notes);
            }
            println!("      Has Notes: {}", reminder.has_notes);
            if let Some(ref cal) = reminder.calendar_title {
                println!("      Calendar/List: {}", cal);
            }
            if let Some(ref ext_id) = reminder.external_identifier {
                println!("      External ID: {}", ext_id);
            }
            if let Some(ref location) = reminder.location {
                println!("      Location: {}", location);
            }
            if let Some(ref url) = reminder.URL {
                println!("      URL: {}", url);
            }
            if let Some(creation_date) = reminder.creation_date {
                println!(
                    "      Creation Date: {}",
                    creation_date.format("%Y-%m-%d %H:%M:%S")
                );
            } else {
                println!("      Creation Date: None");
            }
            if let Some(last_modified_date) = reminder.last_modified_date {
                println!(
                    "      Last Modified Date: {}",
                    last_modified_date.format("%Y-%m-%d %H:%M:%S")
                );
            } else {
                println!("      Last Modified Date: None");
            }
            if let Some(ref timezone) = reminder.timezone {
                println!("      Timezone: {}", timezone);
            }
            println!("      Has Alarms: {}", reminder.has_alarms);
            println!(
                "      Has Recurrence Rules: {}",
                reminder.has_recurrence_rules
            );
            println!("      Has Attendees: {}", reminder.has_attendees);
        }
    }

    if !show_all {
        println!("\nUse --all to see more details.");
    }

    Ok(())
}

fn cmd_reminders_add(
    title: &str,
    notes: Option<&str>,
    list: Option<&str>,
    priority: Option<usize>,
    due_tz: Option<&str>,
) -> Result<(), EventKitError> {
    if let Some(p) = priority
        && p > 9
    {
        eprintln!("Priority must be between 0 and 9");
        return Err(EventKitError::SaveFailed(
            "Invalid priority value".to_string(),
        ));
    }

    let manager = RemindersManager::new();
    let reminder = manager.create_reminder(&eventkit::ReminderDraft {
        title,
        notes,
        calendar_title: list,
        priority,
        due_date_timezone: due_tz,
        ..Default::default()
    })?;

    println!("✓ Created reminder: {}", reminder.title);
    println!("  ID: {}", reminder.identifier);
    if let Some(cal) = reminder.calendar_title {
        println!("  List: {}", cal);
    }

    Ok(())
}

#[allow(non_snake_case)]
fn cmd_reminders_set_URL(id: &str, url: &str) -> Result<(), EventKitError> {
    let manager = RemindersManager::new();
    let value = if url.is_empty() { None } else { Some(url) };
    manager.set_URL(id, value)?;
    println!(
        "✓ {}",
        if value.is_some() {
            "Set URL"
        } else {
            "Cleared URL"
        }
    );
    Ok(())
}

fn cmd_reminders_set_due_tz(id: &str, tz: &str) -> Result<(), EventKitError> {
    let manager = RemindersManager::new();
    let value = if tz.is_empty() { None } else { Some(tz) };
    manager.set_due_date_timezone(id, value)?;
    println!(
        "✓ {}",
        if value.is_some() {
            "Set due-date timezone"
        } else {
            "Cleared due-date timezone"
        }
    );
    Ok(())
}

fn cmd_reminders_set_geofence(
    id: &str,
    title: &str,
    lat: f64,
    lng: f64,
    radius: f64,
    proximity: &str,
) -> Result<(), EventKitError> {
    let proximity = match proximity.to_lowercase().as_str() {
        "enter" | "arrive" | "arrival" => eventkit::AlarmProximity::Enter,
        "leave" | "exit" | "departure" => eventkit::AlarmProximity::Leave,
        other => {
            eprintln!("--proximity must be 'enter' or 'leave', got {other:?}");
            return Err(EventKitError::SaveFailed(
                "Invalid proximity value".to_string(),
            ));
        }
    };
    let manager = RemindersManager::new();
    let loc = eventkit::StructuredLocation {
        title: title.to_string(),
        latitude: lat,
        longitude: lng,
        radius,
    };
    manager.set_geofence(id, Some((&loc, proximity)))?;
    println!("✓ Geofence set on {id}");
    Ok(())
}

fn cmd_reminders_clear_geofence(id: &str) -> Result<(), EventKitError> {
    let manager = RemindersManager::new();
    manager.set_geofence(id, None)?;
    println!("✓ Cleared geofence on {id}");
    Ok(())
}

fn cmd_reminders_update(
    id: &str,
    title: Option<&str>,
    notes: Option<&str>,
    priority: Option<usize>,
    completion_date: Option<&str>,
) -> Result<(), EventKitError> {
    if title.is_none() && notes.is_none() && priority.is_none() && completion_date.is_none() {
        eprintln!("No updates specified. Use --title, --notes, --priority, or --completion-date.");
        return Ok(());
    }

    if let Some(p) = priority
        && p > 9
    {
        eprintln!("Priority must be between 0 and 9");
        return Err(EventKitError::SaveFailed(
            "Invalid priority value".to_string(),
        ));
    }

    // Parse completion_date the same way list filters do.
    let completion_date_patch: Option<Option<DateTime<Local>>> = match completion_date {
        None => None,
        Some("") => Some(None),
        Some(raw) => {
            let naive = NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M")
                .or_else(|_| {
                    chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d")
                        .map(|d| d.and_hms_opt(0, 0, 0).unwrap())
                })
                .map_err(|_| {
                    EventKitError::SaveFailed(format!(
                        "Invalid --completion-date '{raw}'. Use YYYY-MM-DD or YYYY-MM-DD HH:MM."
                    ))
                })?;
            let dt = Local
                .from_local_datetime(&naive)
                .single()
                .ok_or_else(|| EventKitError::SaveFailed("Ambiguous local datetime".into()))?;
            Some(Some(dt))
        }
    };

    let manager = RemindersManager::new();
    let reminder = manager.update_reminder(
        id,
        &eventkit::ReminderPatch {
            title,
            notes,
            priority,
            completion_date: completion_date_patch,
            ..Default::default()
        },
    )?;

    println!("✓ Updated reminder: {}", reminder.title);

    Ok(())
}

fn cmd_reminders_complete(id: &str) -> Result<(), EventKitError> {
    let manager = RemindersManager::new();
    let reminder = manager.complete_reminder(id)?;
    println!("✓ Completed: {}", reminder.title);
    Ok(())
}

fn cmd_reminders_uncomplete(id: &str) -> Result<(), EventKitError> {
    let manager = RemindersManager::new();
    let reminder = manager.uncomplete_reminder(id)?;
    println!("○ Marked incomplete: {}", reminder.title);
    Ok(())
}

fn cmd_reminders_delete(id: &str, force: bool) -> Result<(), EventKitError> {
    let manager = RemindersManager::new();
    let reminder = manager.get_reminder(id)?;

    if !force {
        println!("Delete reminder: \"{}\"?", reminder.title);
        println!("This action cannot be undone. Use --force to skip this prompt.");
        return Ok(());
    }

    manager.delete_reminder(id)?;
    println!("✓ Deleted: {}", reminder.title);

    Ok(())
}

fn cmd_reminders_show(id: &str) -> Result<(), EventKitError> {
    let manager = RemindersManager::new();
    let reminder = manager.get_reminder(id)?;

    println!("Reminder Details:\n");
    println!("  Title:     {}", reminder.title);
    println!(
        "  Status:    {}",
        if reminder.completed {
            "Completed"
        } else {
            "Incomplete"
        }
    );
    println!(
        "  Priority:  {}",
        match reminder.priority {
            0 => "None".to_string(),
            1..=4 => format!("High ({})", reminder.priority),
            5 => "Medium".to_string(),
            _ => format!("Low ({})", reminder.priority),
        }
    );

    if let Some(ref notes) = reminder.notes {
        println!("  Notes:     {}", notes);
    }

    if let Some(ref cal) = reminder.calendar_title {
        println!("  List:      {}", cal);
    }

    println!("  ID:        {}", reminder.identifier);

    Ok(())
}

// ============================================================================
// Events commands
// ============================================================================

fn cmd_events_authorize() -> Result<(), EventKitError> {
    let manager = EventsManager::new();

    println!("Requesting access to Calendar...");

    match manager.request_access() {
        Ok(true) => {
            println!("✓ Access granted!");
            Ok(())
        }
        Ok(false) => {
            println!("✗ Access denied.");
            println!("\nTo grant access, go to:");
            println!("System Settings > Privacy & Security > Calendars");
            Err(EventKitError::AuthorizationDenied)
        }
        Err(e) => {
            println!("✗ Failed to request access: {}", e);
            Err(e)
        }
    }
}

fn cmd_events_calendars() -> Result<(), EventKitError> {
    let manager = EventsManager::new();
    let calendars = manager.list_calendars()?;

    if calendars.is_empty() {
        println!("No calendars found.");
        return Ok(());
    }

    println!("Calendars:\n");

    for cal in calendars {
        let source = cal.source.as_deref().unwrap_or("Unknown");
        let modifiable = if cal.allows_modifications {
            ""
        } else {
            " (read-only)"
        };
        println!("  • {} [{}]{}", cal.title, source, modifiable);
        println!("    ID: {}", cal.identifier);
    }

    if let Ok(default) = manager.default_calendar() {
        println!("\nDefault calendar: {}", default.title);
    }

    Ok(())
}

#[allow(clippy::needless_pass_by_value)]
fn cmd_events_list(
    today: bool,
    days: i64,
    calendar_filter: Option<Vec<String>>,
    show_all: bool,
) -> Result<(), EventKitError> {
    let manager = EventsManager::new();

    let events = if today {
        manager.fetch_today_events()?
    } else if let Some(ref cals) = calendar_filter {
        let cal_refs: Vec<&str> = cals.iter().map(std::string::String::as_str).collect();
        let now = Local::now();
        let end = now + Duration::days(days);
        manager.fetch_events(now, end, Some(&cal_refs))?
    } else {
        manager.fetch_upcoming_events(days)?
    };

    if events.is_empty() {
        println!("No events found.");
        return Ok(());
    }

    println!("Events ({}):\n", events.len());

    let mut current_date = String::new();
    for event in events {
        let event_date = event.start_date.format("%Y-%m-%d").to_string();
        if event_date != current_date {
            current_date = event_date.clone();
            println!("\n  📅 {}", event.start_date.format("%A, %B %d, %Y"));
        }

        let time_str = if event.all_day {
            "All day".to_string()
        } else {
            format!(
                "{} - {}",
                event.start_date.format("%H:%M"),
                event.end_date.format("%H:%M")
            )
        };

        println!("     {} {}", time_str, event.title);

        if show_all {
            if let Some(ref location) = event.location {
                println!("        📍 {}", location);
            }
            if let Some(ref notes) = event.notes {
                let truncated: String = notes.chars().take(50).collect();
                let suffix = if notes.len() > 50 { "..." } else { "" };
                println!("        📝 {}{}", truncated, suffix);
            }
            if let Some(ref cal) = event.calendar_title {
                println!("        🗂  {}", cal);
            }
            println!("        ID: {}", event.identifier);
        }
    }

    if !show_all {
        println!("\nUse --all to see more details.");
    }

    Ok(())
}

fn parse_datetime(s: &str) -> Option<chrono::DateTime<Local>> {
    // Try "YYYY-MM-DD HH:MM" format first
    if let Ok(dt) = NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M") {
        return Local.from_local_datetime(&dt).single();
    }

    // Try "YYYY-MM-DD" format (for all-day events)
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let dt = date.and_hms_opt(0, 0, 0)?;
        return Local.from_local_datetime(&dt).single();
    }

    None
}

#[allow(clippy::too_many_arguments)]
fn cmd_events_add(
    title: &str,
    start_str: &str,
    end_str: Option<&str>,
    duration_mins: i64,
    notes: Option<&str>,
    location: Option<&str>,
    calendar: Option<&str>,
    all_day: bool,
    url: Option<&str>,
    availability: Option<&str>,
) -> Result<(), EventKitError> {
    let start = parse_datetime(start_str).ok_or_else(|| {
        EventKitError::SaveFailed(
            "Invalid start date format. Use YYYY-MM-DD HH:MM or YYYY-MM-DD".to_string(),
        )
    })?;

    let end = if let Some(end_s) = end_str {
        parse_datetime(end_s).ok_or_else(|| {
            EventKitError::SaveFailed(
                "Invalid end date format. Use YYYY-MM-DD HH:MM or YYYY-MM-DD".to_string(),
            )
        })?
    } else if all_day {
        start + Duration::days(1)
    } else {
        start + Duration::minutes(duration_mins)
    };

    // Map --availability to enum at CLI boundary so errors land here.
    let availability_enum = match availability {
        None => None,
        Some("busy") => Some(eventkit::EventAvailability::Busy),
        Some("free") => Some(eventkit::EventAvailability::Free),
        Some("tentative") => Some(eventkit::EventAvailability::Tentative),
        Some("unavailable") => Some(eventkit::EventAvailability::Unavailable),
        Some(other) => {
            return Err(EventKitError::SaveFailed(format!(
                "--availability must be busy|free|tentative|unavailable, got {other:?}"
            )));
        }
    };

    let manager = EventsManager::new();
    let event = manager.create_event(&eventkit::EventDraft {
        title,
        start: Some(start),
        end: Some(end),
        notes,
        location,
        calendar_title: calendar,
        all_day,
        URL: url,
        availability: availability_enum,
        ..Default::default()
    })?;

    println!("✓ Created event: {}", event.title);
    println!("  Start: {}", event.start_date.format("%Y-%m-%d %H:%M"));
    println!("  End:   {}", event.end_date.format("%Y-%m-%d %H:%M"));
    println!("  ID: {}", event.identifier);
    if let Some(cal) = event.calendar_title {
        println!("  Calendar: {}", cal);
    }

    Ok(())
}

fn cmd_events_delete(id: &str, force: bool, span: &str) -> Result<(), EventKitError> {
    let manager = EventsManager::new();
    let event = manager.get_event(id)?;

    if !force {
        println!("Delete event: \"{}\"?", event.title);
        println!("This action cannot be undone. Use --force to skip this prompt.");
        return Ok(());
    }

    let affect_future = match span {
        "this" => false,
        "future" => true,
        other => {
            return Err(EventKitError::SaveFailed(format!(
                "--span must be 'this' or 'future', got {other:?}"
            )));
        }
    };
    manager.delete_event(id, affect_future)?;
    println!(
        "✓ Deleted: {}{}",
        event.title,
        if affect_future {
            " (and future occurrences)"
        } else {
            ""
        }
    );

    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cmd_events_update(
    id: &str,
    title: Option<&str>,
    notes: Option<&str>,
    location: Option<&str>,
    start: Option<&str>,
    end: Option<&str>,
    all_day: Option<bool>,
    calendar: Option<&str>,
    url: Option<&str>,
    availability: Option<&str>,
    span: &str,
) -> Result<(), EventKitError> {
    if title.is_none()
        && notes.is_none()
        && location.is_none()
        && start.is_none()
        && end.is_none()
        && all_day.is_none()
        && calendar.is_none()
        && url.is_none()
        && availability.is_none()
    {
        eprintln!(
            "No updates specified. Use one of --title/--notes/--location/--start/--end/--all-day/--calendar/--url/--availability."
        );
        return Ok(());
    }

    let parse_dt =
        |label: &str, s: Option<&str>| -> Result<Option<DateTime<Local>>, EventKitError> {
            match s {
                None => Ok(None),
                Some(raw) => parse_datetime(raw).map(Some).ok_or_else(|| {
                    EventKitError::SaveFailed(format!(
                        "Invalid {label}: '{raw}'. Use YYYY-MM-DD or YYYY-MM-DD HH:MM."
                    ))
                }),
            }
        };
    let start_dt = parse_dt("--start", start)?;
    let end_dt = parse_dt("--end", end)?;

    let availability_enum = match availability {
        None => None,
        Some("busy") => Some(eventkit::EventAvailability::Busy),
        Some("free") => Some(eventkit::EventAvailability::Free),
        Some("tentative") => Some(eventkit::EventAvailability::Tentative),
        Some("unavailable") => Some(eventkit::EventAvailability::Unavailable),
        Some(other) => {
            return Err(EventKitError::SaveFailed(format!(
                "--availability must be busy|free|tentative|unavailable, got {other:?}"
            )));
        }
    };

    let span_enum = match span {
        "this" => eventkit::EventSpan::This,
        "future" => eventkit::EventSpan::Future,
        other => {
            return Err(EventKitError::SaveFailed(format!(
                "--span must be 'this' or 'future', got {other:?}"
            )));
        }
    };

    fn opt_patch(s: Option<&str>) -> Option<Option<&str>> {
        s.map(|v| if v.is_empty() { None } else { Some(v) })
    }

    let manager = EventsManager::new();
    let updated = manager.update_event(
        id,
        &eventkit::EventPatch {
            title,
            notes: opt_patch(notes),
            location: opt_patch(location),
            start: start_dt,
            end: end_dt,
            all_day,
            calendar_title: calendar,
            URL: opt_patch(url),
            availability: availability_enum,
            structured_location: None,
            span: span_enum,
        },
    )?;
    println!(
        "✓ Updated event: {}{}",
        updated.title,
        if matches!(span_enum, eventkit::EventSpan::Future) {
            " (this and future occurrences)"
        } else {
            ""
        }
    );
    Ok(())
}

#[cfg(feature = "mcp")]
fn cmd_dump(cmd: DumpCommands) -> Result<(), EventKitError> {
    let json = match cmd {
        DumpCommands::Reminder { id } => eventkit::mcp::dump_reminder(&id)?,
        DumpCommands::ReminderRaw { id, values } => eventkit::mcp::dump_reminder_raw(&id, values)?,
        DumpCommands::ReminderPrivate { id } => eventkit::mcp::dump_reminder_private(&id)?,
        DumpCommands::EventPrivate { id } => eventkit::mcp::dump_event_private(&id)?,
        DumpCommands::Reminders { list } => eventkit::mcp::dump_reminders(list.as_deref())?,
        DumpCommands::Event { id } => eventkit::mcp::dump_event(&id)?,
        DumpCommands::Events { days } => eventkit::mcp::dump_events(days)?,
        DumpCommands::ReminderLists => eventkit::mcp::dump_reminder_lists()?,
        DumpCommands::Calendars => eventkit::mcp::dump_calendars()?,
        DumpCommands::Sources => eventkit::mcp::dump_sources()?,
    };
    println!("{json}");
    Ok(())
}

fn cmd_events_show(id: &str) -> Result<(), EventKitError> {
    let manager = EventsManager::new();
    let event = manager.get_event(id)?;

    println!("Event Details:\n");
    println!("  Title:     {}", event.title);
    println!("  Start:     {}", event.start_date.format("%Y-%m-%d %H:%M"));
    println!("  End:       {}", event.end_date.format("%Y-%m-%d %H:%M"));
    println!("  All Day:   {}", if event.all_day { "Yes" } else { "No" });

    if let Some(ref location) = event.location {
        println!("  Location:  {}", location);
    }

    if let Some(ref notes) = event.notes {
        println!("  Notes:     {}", notes);
    }

    if let Some(ref cal) = event.calendar_title {
        println!("  Calendar:  {}", cal);
    }

    println!("  ID:        {}", event.identifier);

    Ok(())
}
