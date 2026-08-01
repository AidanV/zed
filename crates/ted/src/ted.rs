//! `ted` presents Zed's editor as a full-screen terminal application. GPUI runs
//! headlessly as the state, action and layout engine; Ratatui paints the cell
//! grid. See `SPEC.md` for the full design.

pub mod bootstrap;
pub mod cell;
pub mod frame;
pub mod input;
pub mod platform;
pub mod render;
pub mod snapshot;
pub mod text_system;
