use std::ffi::OsStr;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};

const DISPLAY: &str = "wayland-0";
const WSLG_RUNTIME: &str = "/mnt/wslg/runtime-dir";
const WSLG_X11: &str = "/tmp/.X11-unix/X0";

pub(super) fn is_wslg() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
        && Path::new(WSLG_RUNTIME).is_dir()
}

fn fallback_display(
    runtime: Option<&Path>,
    display: Option<&OsStr>,
    socket: Option<&OsStr>,
    fallback: &Path,
) -> Option<PathBuf> {
    if !default_wayland(display, socket) {
        return None;
    }
    if display == Some(OsStr::new(DISPLAY)) && runtime.is_some_and(|dir| is_socket(&dir.join(DISPLAY))) {
        return None;
    }
    let fallback = fallback.join(DISPLAY);
    is_socket(&fallback).then_some(fallback)
}

fn default_wayland(display: Option<&OsStr>, socket: Option<&OsStr>) -> bool {
    socket.is_none_or(OsStr::is_empty) && display.is_none_or(|name| name.is_empty() || name == DISPLAY)
}

fn is_socket(path: &Path) -> bool {
    path.metadata().is_ok_and(|meta| meta.file_type().is_socket())
}

fn use_x11(display: Option<&OsStr>, wayland: Option<&OsStr>, socket: Option<&OsStr>, x11: &Path) -> bool {
    display.is_none_or(|name| name.is_empty() || name == ":0" || name == ":0.0")
        && default_wayland(wayland, socket)
        && is_socket(x11)
}

fn use_wslg_x11() -> bool {
    use_x11(
        std::env::var_os("DISPLAY").as_deref(),
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("WAYLAND_SOCKET").as_deref(),
        Path::new(WSLG_X11),
    )
}

/// Call only at single-threaded startup. Do not change XDG_RUNTIME_DIR: it also hosts unrelated sockets.
pub(super) unsafe fn prepare_display() {
    if !is_wslg() {
        return;
    }
    if use_wslg_x11() {
        if std::env::var_os("DISPLAY").is_none_or(|name| name.is_empty()) {
            unsafe { std::env::set_var("DISPLAY", ":0") };
        }
    } else if let Some(display) = fallback_display(
        std::env::var_os("XDG_RUNTIME_DIR").as_deref().map(Path::new),
        std::env::var_os("WAYLAND_DISPLAY").as_deref(),
        std::env::var_os("WAYLAND_SOCKET").as_deref(),
        Path::new(WSLG_RUNTIME),
    ) {
        unsafe { std::env::set_var("WAYLAND_DISPLAY", display) };
    }
    // Debian's Mesa can ship D3D12 but default to llvmpipe with WSLg's virtual display.
    if std::env::var_os("GALLIUM_DRIVER").is_none()
        && std::env::var_os("WGPU_BACKEND").is_none()
        && Path::new("/dev/dxg").exists()
    {
        unsafe { std::env::set_var("GALLIUM_DRIVER", "d3d12") };
    }
}

pub(super) fn configure_window(native: &mut eframe::NativeOptions) {
    use winit::platform::x11::EventLoopBuilderExtX11;
    if is_wslg() && use_wslg_x11() {
        // The WSLg compositor can disconnect native Wayland clients during buffer presentation.
        native.event_loop_builder = Some(Box::new(|builder| {
            builder.with_x11();
        }));
    }
}

pub(super) fn prefer_accelerated_adapter(setup: &mut eframe::egui_wgpu::WgpuSetupCreateNew) {
    use eframe::egui_wgpu::wgpu::{Backend, DeviceType};
    if !is_wslg() || std::env::var_os("WGPU_BACKEND").is_some() {
        return;
    }
    setup.native_adapter_selector = Some(std::sync::Arc::new(|adapters, surface| {
        adapters
            .iter()
            .filter(|adapter| surface.is_none_or(|surface| adapter.is_surface_supported(surface)))
            .min_by_key(|adapter| {
                let info = adapter.get_info();
                (info.device_type == DeviceType::Cpu, info.backend != Backend::Vulkan)
            })
            .cloned()
            .ok_or_else(|| "No graphics adapter can present to WSLg".into())
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    #[test]
    fn wslg_uses_its_socket_when_the_default_is_missing_but_preserves_custom_displays() {
        let wslg = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let fallback = wslg.path().join(DISPLAY);
        let _fallback = UnixListener::bind(&fallback).unwrap();
        let dir = Some(runtime.path());
        let default = Some(OsStr::new(DISPLAY));
        assert_eq!(fallback_display(dir, default, None, wslg.path()), Some(fallback.clone()));
        assert_eq!(fallback_display(None, None, None, wslg.path()), Some(fallback));
        assert_eq!(fallback_display(dir, Some("wayland-custom".as_ref()), None, wslg.path()), None);
        assert_eq!(fallback_display(dir, default, Some("3".as_ref()), wslg.path()), None);
        let _local = UnixListener::bind(runtime.path().join(DISPLAY)).unwrap();
        assert_eq!(fallback_display(dir, default, None, wslg.path()), None);
    }

    #[test]
    fn wslg_prefers_x11_only_for_the_default_display() {
        let wslg = tempfile::tempdir().unwrap();
        let x11 = wslg.path().join("X0");
        let default = Some(OsStr::new(DISPLAY));
        assert!(!use_x11(None, default, None, &x11));
        let _socket = UnixListener::bind(&x11).unwrap();
        assert!(use_x11(None, default, None, &x11));
        assert!(use_x11(Some(":0".as_ref()), None, None, &x11));
        assert!(!use_x11(Some("localhost:10.0".as_ref()), None, None, &x11));
        assert!(!use_x11(None, Some("/custom/wayland-0".as_ref()), None, &x11));
    }
}
