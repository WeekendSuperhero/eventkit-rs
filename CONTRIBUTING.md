# Contributing to eventkit-rs

Thank you for your interest in contributing to eventkit-rs! This document provides guidelines and information for contributors.

## Code of Conduct

Please be respectful and constructive in all interactions. We want this to be a welcoming community for everyone.

## Getting Started

### Prerequisites

- macOS 10.14 or later
- Rust 1.70 or later
- Xcode Command Line Tools

### Setting Up Development Environment

1. Clone the repository:

   ```bash
   git clone https://github.com/weekendsuperhero/eventkit-rs.git
   cd eventkit-rs
   ```

2. Build the project:

   ```bash
   cargo build
   ```

3. Run tests:

   ```bash
   cargo test
   ```

4. Run clippy for linting:
   ```bash
   cargo clippy --all-targets --all-features
   ```

## How to Contribute

### Reporting Bugs

If you find a bug, please create an issue with:

- A clear, descriptive title
- Steps to reproduce the bug
- Expected behavior vs actual behavior
- Your macOS version and Rust version
- Any relevant error messages or logs

### Suggesting Features

Feature requests are welcome! Please create an issue with:

- A clear description of the feature
- Use cases and benefits
- Any implementation ideas you have

### Pull Requests

1. Fork the repository
2. Create a feature branch: `git checkout -b feature/my-new-feature`
3. Make your changes
4. Add or update tests as needed
5. Ensure all tests pass: `cargo test`
6. Ensure code passes clippy: `cargo clippy`
7. Format your code: `cargo fmt`
8. Commit your changes with a descriptive message
9. Push to your fork
10. Open a Pull Request

### Pull Request Guidelines

- Keep changes focused and atomic
- Update documentation for any API changes
- Add tests for new functionality
- Follow existing code style and conventions
- Write clear commit messages

## Code Style

- Follow standard Rust style guidelines
- Use `cargo fmt` for formatting
- Use `cargo clippy` and address all warnings
- Document public APIs with doc comments
- Use meaningful variable and function names

## Testing

### Running Tests

```bash
# Run all tests
cargo test

# Run tests with output
cargo test -- --nocapture

# Run a specific test
cargo test test_name
```

### Writing Tests

- Add unit tests in the same file as the code being tested
- Add integration tests in `tests/` directory
- Test both success and error cases
- Use descriptive test names

Note: Some tests may require calendar/reminders access permissions on macOS.

## Documentation

- Update README.md for user-facing changes
- Add doc comments (`///`) for public APIs
- Include examples in doc comments where helpful
- Run `cargo doc --open` to preview documentation

## Release Process

Releases are managed by maintainers. The process is:

1. Update version in `Cargo.toml`
2. Update CHANGELOG.md
3. Create a git tag: `git tag v0.x.x`
4. Push the tag: `git push origin v0.x.x`
5. GitHub Actions will publish to crates.io

## Questions?

If you have questions, feel free to:

- Open an issue for discussion
- Check existing issues and PRs

Thank you for contributing!
