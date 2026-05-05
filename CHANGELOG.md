# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Initial scaffolding: `#![cfg(target_os = "macos")]` crate that
  dlopens VideoToolbox + CoreVideo + CoreMedia + CoreFoundation
  via `libloading` on first use. Smoke test verifies symbol resolution
  for `VTDecompressionSessionCreate` + `CFRetain`.
- Unified `register(&mut RuntimeContext)` entry point matching the
  framework convention — no-op in round 1 (no factories yet).
- Standalone-friendly: default-on `registry` feature gates the
  `oxideav-core` dep + the `register` fn.
- README documents the priority-0 placement (hardware preferred over
  pure-Rust) and the planned `--no-hwaccel` CLI opt-out.
