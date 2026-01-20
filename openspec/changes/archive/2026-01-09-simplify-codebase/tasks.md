# Tasks

## 1. Inline `extract_util` closure in `check_notifications`

- [x] **Completed**

**File:** `src/lib.rs:267-271`

Removed the `extract_util` closure and inlined the map calls directly.

---

## 2. Remove unnecessary braces in `handle_successful_fetch`

- [x] **Completed**

**File:** `src/service.rs:577-579`

Removed unnecessary `{}` blocks around independent await statements.

---

## 3. Extract `spawn_load_credentials` helper

- [x] **Completed**

**File:** `src/service.rs:148-157`

Created helper function and updated all three call sites to use `let Some(creds_result) = spawn_load_credentials().await else { return ... }` pattern.

---

## 4. Use iterator for optional menu items

- [x] **Completed**

**File:** `src/tray.rs:695-697`

Replaced repetitive if-let pattern with `.into_iter().flatten()` per clippy recommendation.

---

## 5. Clean up `#[allow(unused)]` in test modules

- [x] **Completed**

**File:** `src/api.rs`

Removed `#![allow(unused)]` module attributes and cleaned up unused imports (`TimeZone`, `Timelike`).

---

## 6. Final validation

- [x] **Completed**

All validation passed:
- `cargo check` - ✓
- `cargo clippy -- -D warnings` - ✓
- `cargo test` - ✓ (32 unit tests + 3 integration tests passed)
