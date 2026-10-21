//! Native renderer bootstrap before any application windows are created.
//!
//! Windows exposes its login session through the `SESSIONNAME` environment variable.
//! This native boundary passes the detected session kind to the pure renderer policy.
//! Slint's process-global backend is installed before creating application windows.

use anyhow::{Context as _, Result};

use crate::cli::RendererPreference;

pub(super) fn select_renderer(preference: RendererPreference) -> Result<()> {
    let Some(renderer_name) = preferred_renderer(preference, is_remote_session()) else {
        return Ok(());
    };

    slint::BackendSelector::new()
        .backend_name("winit".into())
        .renderer_name(renderer_name.into())
        .select()
        .with_context(|| format!("failed to initialize the {renderer_name} renderer"))
}

pub(super) const fn preferred_renderer(
    preference: RendererPreference,
    remote_session: bool,
) -> Option<&'static str> {
    if matches!(preference, RendererPreference::Auto) && remote_session {
        Some("software")
    } else {
        renderer_name(preference)
    }
}

pub(super) const fn renderer_name(preference: RendererPreference) -> Option<&'static str> {
    match preference {
        RendererPreference::Auto => None,
        RendererPreference::Gpu => Some(platform_gpu_renderer()),
        RendererPreference::Software => Some("software"),
    }
}

#[cfg(windows)]
fn is_remote_session() -> bool {
    // RDP sets SESSIONNAME to "RDP-Tcp#<n>"; the physical console uses "Console".
    std::env::var_os("SESSIONNAME").is_some_and(|name| {
        name.to_string_lossy()
            .to_ascii_lowercase()
            .starts_with("rdp-")
    })
}

#[cfg(not(windows))]
const fn is_remote_session() -> bool {
    false
}

const fn platform_gpu_renderer() -> &'static str {
    if cfg!(target_os = "macos") {
        "skia"
    } else {
        "femtovg"
    }
}
