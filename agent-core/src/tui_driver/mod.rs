//! Self-contained virtual terminal emulation, observation, input encoding, and
//! cell rasterization for the `tui` command.
//!
//! This module owns no PTY processes itself; higher layers pair a
//! [`TerminalEmulator`] and [`FrameWaiter`] with a PTY job stream, encode
//! agent actions through [`input`], inspect panes and interactive elements via
//! [`observe`], and render compact visual frames with [`CellRasterizer`].

pub mod emulator;
pub mod input;
pub mod observe;
pub mod rasterizer;

pub use emulator::{
    Cell, CellStyle, Color, CursorPosition, DEFAULT_COLS, DEFAULT_ROWS, DEFAULT_SCROLLBACK_LIMIT,
    DEFAULT_SETTLE_QUIET_MS, DEFAULT_SETTLE_TIMEOUT_MS, FrameWaiter, MouseTrackingMode,
    ScreenSnapshot, SettleOutcome, TerminalEmulator, TerminalModes,
};
pub use input::{
    KeyPress, Modifiers, TuiInputError, encode_key, encode_mouse_click, encode_mouse_double_click,
    encode_mouse_drag, encode_mouse_right_click, encode_mouse_scroll, encode_type,
};
pub use observe::{
    CellRect, ElementKind, ElementTable, Observation, ObservedElement, ObservedPane,
    RefResolveError, TextTargetError, TuiElementRef, find_unique_text, observe_screen,
    render_observation,
};
pub use rasterizer::{CellMetrics, CellRasterizer, RasterizeError};
