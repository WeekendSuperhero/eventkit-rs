# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/weekendsuperhero/eventkit-rs/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/weekendsuperhero/eventkit-rs/releases/tag/v0.1.0
