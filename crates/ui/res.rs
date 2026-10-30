// Shared by the build scripts of `nothing` and `nothing-setup` via `include!`: generates
// the app icon procedurally (no binary assets in the repo) and embeds it plus the
// application manifest (PerMonitorV2 DPI, Common Controls v6) as Win32 resources.


/// Coverage of one pixel by the logo, supersampled 4x4 for anti-aliasing.
/// The logo is a magnifier whose lens is an empty ring — a zero. "Nothing".
fn coverage(px: f32, py: f32, size: f32) -> (f32, f32) {
    let mut ring = 0.0;
    let mut handle = 0.0;
    for sy in 0..4 {
        for sx in 0..4 {
            let x = (px + (sx as f32 + 0.5) / 4.0) / size;
            let y = (py + (sy as f32 + 0.5) / 4.0) / size;
            // Lens: ring centred at (0.42, 0.42).
            let (cx, cy) = (0.42, 0.42);
            let d = ((x - cx).powi(2) + (y - cy).powi(2)).sqrt();
            let (r_out, r_in) = (0.34, if size <= 16.0 { 0.20 } else { 0.235 });
            if d <= r_out && d >= r_in {
                ring += 1.0 / 16.0;
            }
            // Handle: thick segment from the ring towards the bottom-right corner.
            let (ax, ay, bx, by) = (0.66, 0.66, 0.90, 0.90);
            let (vx, vy) = (bx - ax, by - ay);
            let t = (((x - ax) * vx + (y - ay) * vy) / (vx * vx + vy * vy)).clamp(0.0, 1.0);
            let (qx, qy) = (ax + t * vx, ay + t * vy);
            let hd = ((x - qx).powi(2) + (y - qy).powi(2)).sqrt();
            if hd <= if size <= 16.0 { 0.10 } else { 0.085 } && d > r_in {
                handle += 1.0 / 16.0;
            }
        }
    }
    (ring, handle)
}

fn bmp_entry(size: u32) -> Vec<u8> {
    let mut px = Vec::with_capacity((size * size * 4) as usize);
    // Bottom-up rows, BGRA.
    for row in (0..size).rev() {
        for col in 0..size {
            let (ring, handle) = coverage(col as f32, row as f32, size as f32);
            let a = (ring + handle).min(1.0);
            // Ring: warm orange; handle: slightly darker. Premultiplication not needed for ICO.
            let (r, g, b) = if handle > ring { (0xD9u8, 0x5Au8, 0x0Bu8) } else { (0xFFu8, 0x8Au8, 0x1Fu8) };
            px.extend_from_slice(&[b, g, r, (a * 255.0).round() as u8]);
        }
    }
    let mask_row = size.div_ceil(32) * 4;
    let mut out = Vec::new();
    let le32 = |v: u32| v.to_le_bytes();
    out.extend_from_slice(&le32(40));
    out.extend_from_slice(&le32(size));
    out.extend_from_slice(&le32(size * 2));
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&32u16.to_le_bytes());
    out.extend_from_slice(&[0u8; 24]);
    out.extend_from_slice(&px);
    out.extend(std::iter::repeat_n(0u8, (mask_row * size) as usize));
    out
}

fn make_ico(path: &std::path::Path) {
    let sizes = [16u32, 20, 24, 32, 40, 48, 64, 128];
    let images: Vec<Vec<u8>> = sizes.iter().map(|&s| bmp_entry(s)).collect();
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * sizes.len() as u32;
    for (s, img) in sizes.iter().zip(&images) {
        ico.push(if *s >= 256 { 0 } else { *s as u8 });
        ico.push(if *s >= 256 { 0 } else { *s as u8 });
        ico.extend_from_slice(&[0, 0]);
        ico.extend_from_slice(&1u16.to_le_bytes());
        ico.extend_from_slice(&32u16.to_le_bytes());
        ico.extend_from_slice(&(img.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += img.len() as u32;
    }
    for img in images {
        ico.extend_from_slice(&img);
    }
    std::fs::write(path, ico).unwrap();
}

const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity type="win32" name="Nothing" version="0.1.0.0"/>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity type="win32" name="Microsoft.Windows.Common-Controls" version="6.0.0.0"
        processorArchitecture="*" publicKeyToken="6595b64144ccf1df" language="*"/>
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </windowsSettings>
  </application>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security><requestedPrivileges><requestedExecutionLevel level="asInvoker" uiAccess="false"/></requestedPrivileges></security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application><supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/></application>
  </compatibility>
</assembly>
"#;

/// Write icon + manifest into OUT_DIR and link them into the executable.
fn embed_resources() {
    let out = std::path::PathBuf::from(std::env::var("OUT_DIR").unwrap());
    let ico = out.join("app.ico");
    let manifest = out.join("app.manifest");
    make_ico(&ico);
    std::fs::write(&manifest, MANIFEST).unwrap();
    let esc = |p: &std::path::Path| p.display().to_string().replace('\\', "\\\\");
    let rc = out.join("app.rc");
    std::fs::write(&rc, format!("1 ICON \"{}\"\n1 24 \"{}\"\n", esc(&ico), esc(&manifest))).unwrap();
    embed_resource::compile(&rc, embed_resource::NONE).manifest_required().unwrap();
}
