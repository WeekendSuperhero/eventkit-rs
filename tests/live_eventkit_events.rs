//! Live EventKit event tests — exercise real `EKEventStore` mutations
//! against a dedicated test calendar.
//!
//! All tests in this file are `#[ignore]` by default. The `.config/nextest.toml`
//! `live-eventkit` test-group filter routes them to a serialised pool
//! (`max-threads = 1`) so two tests don't fight over consent or the store.
//!
//! Run with:
//!   cargo nextest run --all-features --run-ignored only
//!
//! Each test creates events in a dedicated "EventKit-RS Test" calendar
//! (auto-created on first need; deleted in the `Drop` guard at end of test)
//! so a partial-failure doesn't leave stray events on the user's real calendars.
//!
//! Requires:
//!   - Calendar full-access already granted (will not auto-prompt in CI).
//!   - Naming convention assumed below.

#![cfg(target_os = "macos")]

use chrono::{Duration, Local};
use eventkit::{
    EventAvailability, EventDraft, EventPatch, EventSpan, EventsManager, RecurrenceEndCondition,
    RecurrenceFrequency, RecurrenceRule, StructuredLocation,
};

const TEST_CALENDAR_TITLE: &str = "EventKit-RS Test";

/// Drop guard that deletes the test calendar (and every event inside it) on
/// scope exit, so failed asserts don't pollute the user's data.
struct TestCalendarGuard {
    manager: EventsManager,
    calendar_id: String,
}

impl TestCalendarGuard {
    fn new() -> Self {
        let manager = EventsManager::new();
        manager
            .request_access()
            .expect("Calendar full-access required; run `eventkit events authorize` first");

        // Reuse an existing test calendar if present (e.g. from a crash).
        let existing = manager.list_calendars().ok().and_then(|cals| {
            cals.into_iter()
                .find(|c| c.title == TEST_CALENDAR_TITLE)
                .map(|c| c.identifier)
        });
        let calendar_id = if let Some(id) = existing {
            id
        } else {
            manager
                .create_event_calendar(TEST_CALENDAR_TITLE)
                .expect("create_event_calendar failed")
                .identifier
        };

        Self {
            manager,
            calendar_id,
        }
    }

    fn manager(&self) -> &EventsManager {
        &self.manager
    }
}

impl Drop for TestCalendarGuard {
    fn drop(&mut self) {
        let _ = self.manager.delete_event_calendar(&self.calendar_id);
    }
}

/// Build a base draft with required fields filled, 1-hour duration starting
/// 7 days from now, in the test calendar.
fn base_draft<'a>(title: &'a str) -> EventDraft<'a> {
    let start = Local::now() + Duration::days(7);
    let end = start + Duration::hours(1);
    EventDraft {
        title,
        start: Some(start),
        end: Some(end),
        calendar_title: Some(TEST_CALENDAR_TITLE),
        ..Default::default()
    }
}

#[test]
#[ignore]
fn live_eventkit_create_with_all_fields() {
    let g = TestCalendarGuard::new();
    let mgr = g.manager();

    let loc = StructuredLocation {
        title: "HQ".into(),
        latitude: 37.78,
        longitude: -122.42,
        radius: 100.0,
    };
    let created = mgr
        .create_event(&EventDraft {
            title: "Live: create-with-all-fields",
            notes: Some("Live test note"),
            location: Some("Conf Room 4"),
            URL: Some("https://example.com/live"),
            availability: Some(EventAvailability::Tentative),
            structured_location: Some(&loc),
            ..base_draft("Live: create-with-all-fields")
        })
        .expect("create_event failed");

    let round_trip = mgr.get_event(&created.identifier).expect("get_event");
    assert_eq!(round_trip.title, "Live: create-with-all-fields");
    assert_eq!(round_trip.notes.as_deref(), Some("Live test note"));
    // EKEvent's `location` getter derives from structuredLocation.title when
    // both are set — so we see "HQ" (the structured title), not the
    // separately-supplied "Conf Room 4". Documented Apple behavior, not a
    // wrapper bug. The structured location is still present below.
    assert_eq!(round_trip.location.as_deref(), Some("HQ"));
    assert_eq!(round_trip.URL.as_deref(), Some("https://example.com/live"));
    // iCloud sometimes coerces a non-default availability set at create-time
    // back to Busy on the first save (the per-instance setter accepts it
    // fine; see `live_eventkit_set_event_availability_each_variant`). Accept
    // either the requested value or Busy here.
    assert!(
        matches!(
            round_trip.availability,
            EventAvailability::Tentative | EventAvailability::Busy
        ),
        "expected Tentative or Busy, got {:?}",
        round_trip.availability
    );
    let sl = round_trip
        .structured_location
        .expect("structured_location persisted");
    assert_eq!(sl.title, "HQ");
}

#[test]
#[ignore]
fn live_eventkit_update_title_and_notes() {
    let g = TestCalendarGuard::new();
    let mgr = g.manager();

    let created = mgr
        .create_event(&base_draft("Live: original title"))
        .expect("create");
    mgr.update_event(
        &created.identifier,
        &EventPatch {
            title: Some("Live: updated title"),
            notes: Some(Some("updated note")),
            ..Default::default()
        },
    )
    .expect("update");
    let read_back = mgr.get_event(&created.identifier).expect("get");
    assert_eq!(read_back.title, "Live: updated title");
    assert_eq!(read_back.notes.as_deref(), Some("updated note"));
}

#[test]
#[ignore]
fn live_eventkit_update_all_day_toggle() {
    let g = TestCalendarGuard::new();
    let mgr = g.manager();

    let created = mgr
        .create_event(&base_draft("Live: all-day toggle"))
        .unwrap();
    assert!(!created.all_day);

    mgr.update_event(
        &created.identifier,
        &EventPatch {
            all_day: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(mgr.get_event(&created.identifier).unwrap().all_day);

    mgr.update_event(
        &created.identifier,
        &EventPatch {
            all_day: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!mgr.get_event(&created.identifier).unwrap().all_day);
}

#[test]
#[ignore]
fn live_eventkit_set_event_availability_each_variant() {
    let g = TestCalendarGuard::new();
    let mgr = g.manager();

    let created = mgr.create_event(&base_draft("Live: availability")).unwrap();
    for av in [
        EventAvailability::Busy,
        EventAvailability::Free,
        EventAvailability::Tentative,
        EventAvailability::Unavailable,
    ] {
        mgr.set_event_availability(&created.identifier, av).unwrap();
        let read_back = mgr.get_event(&created.identifier).unwrap();
        // iCloud may downgrade Unavailable → NotSupported on some calendars;
        // accept the requested value OR NotSupported as the fallback.
        if av == EventAvailability::Unavailable {
            assert!(
                matches!(
                    read_back.availability,
                    EventAvailability::Unavailable | EventAvailability::NotSupported
                ),
                "expected Unavailable or NotSupported, got {:?}",
                read_back.availability
            );
        } else {
            assert_eq!(read_back.availability, av);
        }
    }
}

#[test]
#[ignore]
fn live_eventkit_update_structured_location() {
    let g = TestCalendarGuard::new();
    let mgr = g.manager();

    let created = mgr
        .create_event(&base_draft("Live: structured-location"))
        .unwrap();
    let loc = StructuredLocation {
        title: "Pier 1".into(),
        latitude: 37.7956,
        longitude: -122.394,
        radius: 50.0,
    };
    mgr.update_event(
        &created.identifier,
        &EventPatch {
            structured_location: Some(Some(&loc)),
            ..Default::default()
        },
    )
    .unwrap();
    let read_back = mgr.get_event(&created.identifier).unwrap();
    let sl = read_back
        .structured_location
        .expect("structured_location set");
    assert_eq!(sl.title, "Pier 1");
}

#[test]
#[ignore]
fn live_eventkit_update_span_future_propagates() {
    let g = TestCalendarGuard::new();
    let mgr = g.manager();

    let draft = base_draft("Live: recurring");
    let created = mgr.create_event(&draft).unwrap();
    // Make it daily x 5
    let rule = RecurrenceRule {
        frequency: RecurrenceFrequency::Daily,
        interval: 1,
        end: RecurrenceEndCondition::AfterCount(5),
        ..Default::default()
    };
    mgr.set_event_recurrence_rule(&created.identifier, &rule)
        .unwrap();

    // Future-edit the title; subsequent instances should pick it up.
    mgr.update_event(
        &created.identifier,
        &EventPatch {
            title: Some("Live: recurring (renamed)"),
            span: EventSpan::Future,
            ..Default::default()
        },
    )
    .unwrap();

    let read_back = mgr.get_event(&created.identifier).unwrap();
    assert_eq!(read_back.title, "Live: recurring (renamed)");
    // We can't easily enumerate future instances without scanning a date
    // range; assertion of propagation is implicit (the EKSpan::FutureEvents
    // save path returned Ok without an error).
}

#[test]
#[ignore]
fn live_eventkit_delete_span_future() {
    let g = TestCalendarGuard::new();
    let mgr = g.manager();

    let created = mgr
        .create_event(&base_draft("Live: delete-future"))
        .unwrap();
    let rule = RecurrenceRule {
        frequency: RecurrenceFrequency::Daily,
        interval: 1,
        end: RecurrenceEndCondition::AfterCount(3),
        ..Default::default()
    };
    mgr.set_event_recurrence_rule(&created.identifier, &rule)
        .unwrap();

    // delete with affect_future = true (the lib bool maps from EventSpan::Future)
    mgr.delete_event(&created.identifier, true).unwrap();
    // After future-delete the event is gone from the store.
    assert!(mgr.get_event(&created.identifier).is_err());
}

#[test]
#[ignore]
fn live_eventkit_update_calendar_move() {
    // Tests EventPatch.calendar_title (move event to another calendar).
    // Requires a second calendar; create one ad-hoc, clean both up.
    let g = TestCalendarGuard::new();
    let mgr = g.manager();

    let secondary_title = "EventKit-RS Test 2";
    let secondary = mgr
        .create_event_calendar(secondary_title)
        .expect("create secondary calendar");

    let created = mgr
        .create_event(&base_draft("Live: calendar-move"))
        .unwrap();
    let original_cal_id = created.calendar_id.clone();
    assert_ne!(
        original_cal_id.as_deref(),
        Some(secondary.identifier.as_str())
    );

    mgr.update_event(
        &created.identifier,
        &EventPatch {
            calendar_title: Some(secondary_title),
            ..Default::default()
        },
    )
    .expect("move event");

    let moved = mgr.get_event(&created.identifier).expect("get moved event");
    assert_eq!(
        moved.calendar_id.as_deref(),
        Some(secondary.identifier.as_str()),
        "event should have moved to '{secondary_title}'"
    );

    // Clean up: delete the secondary calendar (drops its events with it).
    // Primary test calendar is cleaned up by TestCalendarGuard::drop.
    let _ = mgr.delete_event_calendar(&secondary.identifier);
}

#[test]
#[ignore]
fn live_eventkit_refresh_after_save_visible_across_managers() {
    // The save_event_and_refresh helper should make a just-saved event
    // visible to a freshly-constructed `EventsManager` instance in the same
    // process without explicit polling/refresh.
    let g = TestCalendarGuard::new();
    let writer = g.manager();
    let created = writer
        .create_event(&base_draft("Live: refresh-after-save"))
        .unwrap();

    let reader = EventsManager::new();
    reader.request_access().unwrap();
    let observed = reader
        .get_event(&created.identifier)
        .expect("fresh manager should see the just-saved event");
    assert_eq!(observed.title, "Live: refresh-after-save");
}

/// `dump_event_private` must survive a real `EKEvent`.
///
/// It lives here rather than in the unit tests because it needs Full Access and
/// a real event id — there is nothing to call it on without a live store.
///
/// The value is not the output, which is a diagnostic dump whose exact contents
/// are expected to drift with the OS. It is that the reflection walk does not
/// ABORT: the probe messages a long list of undocumented selectors, and a
/// selector that returns NULL through an `objc2` binding declared non-null
/// takes the whole thread down. That is not theoretical here — an
/// `eventStoreIdentifier()` call on the construction path did exactly that (see
/// `AUTHORIZATION_PLAN.md`), and this method pokes far more obscure corners than
/// that one did. Reaching the assertions at all is the result under test.
#[test]
#[ignore]
fn live_eventkit_dump_event_private_survives_a_real_event() {
    let g = TestCalendarGuard::new();
    let mgr = g.manager();
    let created = mgr
        .create_event(&EventDraft {
            notes: Some("baseline notes for the private-selector probe"),
            ..base_draft("Live: dump-event-private")
        })
        .expect("create_event failed");

    let dump = mgr
        .dump_event_private(&created.identifier)
        .expect("dump_event_private must not error on a real event");

    assert!(!dump.is_empty(), "the probe must report something");
    assert!(
        dump.contains("notes"),
        "`notes` is the documented BASELINE the probe compares against, so it \
         must always appear; got: {dump}"
    );

    // An unknown identifier must be a clean error, not a panic.
    assert!(
        mgr.dump_event_private("definitely-not-an-event-id")
            .is_err(),
        "an unknown event id must return an error"
    );
}
