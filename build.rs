//! Build script for Wisp.
//!
//! Two jobs:
//!  * Rasterise `assets/icon.svg` into a multi-size Windows `.ico` and the raw
//!    32x32 RGBA the tray needs. The SVG is the single source of truth; nothing
//!    generated is committed. Everything lands in `OUT_DIR`.
//!  * Embed the executable icon plus Windows file metadata via `winresource`.

use std::path::{Path, PathBuf};

/// Sizes packed into the `.ico`. 16/32 cover the title bar and taskbar; the
/// larger frames cover Alt-Tab and the Explorer preview.
const ICO_SIZES: &[u32] = &[16, 24, 32, 48, 64, 128, 256];

/// The icon artwork is authored in a 64x64 viewBox.
const VIEWBOX: f32 = 64.0;

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.svg");
    println!("cargo:rerun-if-changed=build.rs");

    set_main_thread_stack();

    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR is set"));
    let ico_path = out_dir.join("icon.ico");
    let tray_path = out_dir.join("tray-32.rgba");

    generate_icons(&ico_path, &tray_path);

    #[cfg(windows)]
    embed_windows_metadata(&ico_path);
}

/// Reserve a generous stack for the main thread.
///
/// MSVC's default is 1 MiB, which a GPUI window is not obliged to fit inside: the
/// element tree is built by deeply nested generic builders, and a debug build
/// keeps every one of those temporaries on the stack. Rendering a real screen was
/// enough to exhaust it and abort with `STATUS_STACK_OVERFLOW`.
fn set_main_thread_stack() {
    /// 8 MiB, matching what the toolbar and sidebar rendering expects.
    const RESERVE: u32 = 8 * 1024 * 1024;

    let env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    let arg = if env == "msvc" {
        format!("/STACK:{RESERVE}")
    } else {
        format!("-Wl,--stack,{RESERVE}")
    };
    println!("cargo:rustc-link-arg-bins={arg}");
}

/// Rasterise the SVG and write the `.ico` and the tray pixels.
fn generate_icons(ico_path: &Path, tray_path: &Path) {
    let svg = std::fs::read_to_string("assets/icon.svg")
        .expect("assets/icon.svg should be readable");
    let tree = resvg::usvg::Tree::from_str(&svg, &resvg::usvg::Options::default())
        .expect("assets/icon.svg should be valid SVG");

    let ico = build_ico(&tree);
    std::fs::write(ico_path, ico).expect("the .ico should be writable");

    let tray = render_pixmap(&tree, 32);
    std::fs::write(tray_path, demultiply(tray.data())).expect("the tray pixels should be writable");
}

/// Build a complete `.ico` file by hand: an `ICONDIR` header, one 16-byte
/// directory entry per size, then the PNG payloads. Windows Vista and later read
/// PNG-compressed frames directly, which keeps 128/256 small.
fn build_ico(tree: &resvg::usvg::Tree) -> Vec<u8> {
    let mut pngs: Vec<(u32, Vec<u8>)> = Vec::with_capacity(ICO_SIZES.len());
    for &size in ICO_SIZES {
        let pixmap = render_pixmap(tree, size);
        let png = pixmap
            .encode_png()
            .expect("a rendered frame should encode to PNG");
        pngs.push((size, png));
    }

    let mut out = Vec::new();

    // ICONDIR: the counts and offsets below all come from a handful of frames,
    // so the narrowing conversions are infallible here by construction.
    let frame_count = u16::try_from(pngs.len()).expect("the icon has at most 65535 frames");
    let header_len = 6 + 16 * u32::try_from(pngs.len()).expect("frame count fits u32");
    out.extend_from_slice(&0u16.to_le_bytes()); // reserved
    out.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    out.extend_from_slice(&frame_count.to_le_bytes()); // image count

    // ICONDIRENTRY per frame.
    let mut offset = header_len;
    for (size, png) in &pngs {
        // 256 is encoded as 0 in the single-byte width/height fields.
        let dimension = if *size >= 256 {
            0
        } else {
            u8::try_from(*size).expect("an icon frame is at most 256 px")
        };
        let payload = u32::try_from(png.len()).expect("a frame fits u32");
        out.push(dimension); // width
        out.push(dimension); // height
        out.push(0); // colour count (0 = truecolour)
        out.push(0); // reserved
        out.extend_from_slice(&1u16.to_le_bytes()); // colour planes
        out.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
        out.extend_from_slice(&payload.to_le_bytes()); // payload size
        out.extend_from_slice(&offset.to_le_bytes()); // payload offset
        offset += payload;
    }

    for (_, png) in &pngs {
        out.extend_from_slice(png);
    }
    out
}

/// Render the tree into a square pixmap of the given side length.
#[expect(
    clippy::cast_precision_loss,
    reason = "icon sizes are small integers, exact in f32"
)]
fn render_pixmap(tree: &resvg::usvg::Tree, size: u32) -> resvg::tiny_skia::Pixmap {
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(size, size).expect("a non-zero pixmap size");
    let scale = size as f32 / VIEWBOX;
    resvg::render(
        tree,
        resvg::tiny_skia::Transform::from_scale(scale, scale),
        &mut pixmap.as_mut(),
    );
    pixmap
}

/// tiny-skia stores premultiplied RGBA; the tray backend wants straight RGBA.
fn demultiply(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    let mut i = 0;
    while i + 4 <= rgba.len() {
        let (r, g, b, alpha) = (rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]);
        if alpha == 0 {
            out.extend_from_slice(&[0, 0, 0, 0]);
        } else {
            let straight = |channel: u8| {
                let value = (u16::from(channel) * 255) / u16::from(alpha);
                u8::try_from(value.min(255)).expect("clamped to 255")
            };
            out.extend_from_slice(&[straight(r), straight(g), straight(b), alpha]);
        }
        i += 4;
    }
    out
}

/// Attach Windows file metadata and the executable icon.
#[cfg(windows)]
fn embed_windows_metadata(ico_path: &Path) {
    let mut res = winresource::WindowsResource::new();
    res.set("ProductName", "Wisp");
    res.set("FileDescription", "Wisp");
    res.set("CompanyName", "AbrahamX3");
    res.set("LegalCopyright", "Copyright (c) 2026 AbrahamX3");
    res.set("OriginalFilename", "wisp.exe");
    res.set_icon(ico_path.to_str().expect("the icon path should be valid UTF-8"));
    if let Err(error) = res.compile() {
        eprintln!("winresource failed: {error}");
    }
}
