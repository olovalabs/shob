//! Window-resize regression tests.
//!
//! A resize is the one event that re-lays-out everything at once: the
//! resizable sidebar panels, the terminal grid (cols/rows recomputed from
//! the new bounds) and the client-drawn title bar. These tests drive the
//! real view through the same window options `main` uses and draw a frame
//! after every resize step.

include!("resize_tests/01_item.rs");
include!("resize_tests/02_new_sessions_follow_the_selected_layout.rs");
include!("resize_tests/03_saving_a_workspace_records_each_panes_own_model.rs");
