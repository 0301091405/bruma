//! Reading the screen through Desktop Duplication.
//!
//! GDI hands over the screen in step with the compositor: 25 to 30 ms, however
//! little has changed. Desktop Duplication hands over the compositor's own
//! picture: nothing at all when nothing changed, a few milliseconds when
//! something did.
//!
//! It also says which parts of the screen changed, so only those are copied;
//! the recorder keeps only those for GIFs.
//!
//! It is fussier, though. Anything that is not the plain case (a rotated
//! monitor, a picture of another size than its monitor, an output that will
//! not be duplicated) makes [`Duplicator::new`] decline, and any error later
//! makes [`Duplicator::refresh`] give up; the caller then reads the screen
//! with GDI, as before.

use bruma_core::clip::Area;
use windows::Win32::Foundation::{HMODULE, RECT};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_MODE_ROTATION_IDENTITY, DXGI_MODE_ROTATION_UNSPECIFIED,
    DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_MOVE_RECT,
    IDXGIFactory1, IDXGIOutput1, IDXGIOutput5, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CMONITORS};
use windows::core::Interface;

use super::windows::{Grabber, Raw, desktop};

/// How long a monitor may take to hand over its first picture.
const FIRST_PICTURE_MS: u32 = 250;

/// One monitor being duplicated.
struct Output {
    duplication: IDXGIOutputDuplication,
    context: ID3D11DeviceContext,
    /// A texture the processor can read, which the monitor's picture is
    /// copied into.
    staging: ID3D11Texture2D,
    /// Where the monitor lies in the picture of the whole desktop.
    at: (usize, usize),
    size: (usize, usize),
    /// Whether its picture has arrived at least once.
    seen: bool,
    /// Whether a picture that was really drawn has arrived. The first one
    /// handed over can be empty (black) until the screen is next drawn.
    drawn: bool,
    /// The size of the picture as handed over, which on a monitor turned on
    /// its side is the desktop's size turned.
    texture: (usize, usize),
    /// Quarter turns that bring the picture handed over to the desktop's
    /// way up: 0 for a monitor the usual way up; `None` until the first
    /// picture of a turned monitor has been compared with the screen.
    turn: Option<u8>,
}

/// Where in a picture of `texture` size turned `turn` quarter turns the
/// desktop's pixel `(x, y)` is.
fn texel(turn: u8, x: usize, y: usize, texture: (usize, usize)) -> (usize, usize) {
    let (tw, th) = texture;
    match turn {
        1 => (y, th - 1 - x),
        2 => (tw - 1 - x, th - 1 - y),
        3 => (tw - 1 - y, x),
        _ => (x, y),
    }
}

/// Whether a picture of `texture` size turned `turn` quarter turns has the
/// desktop size `size`.
fn fits(turn: u8, texture: (usize, usize), size: (usize, usize)) -> bool {
    if turn % 2 == 1 {
        texture == (size.1, size.0)
    } else {
        texture == size
    }
}

/// Which quarter turns bring the picture `texture` (rows `pitch` bytes long,
/// of size `tsize`) to what the screen shows, `screen` (rows `stride` bytes,
/// the monitor's area `area` in it): compared at a grid of points. `None`
/// when no turn matches well, or when more than one does (a screen of one
/// colour says nothing).
fn find_turn(
    texture: &[u8],
    pitch: usize,
    tsize: (usize, usize),
    screen: &[u8],
    stride: usize,
    area: (usize, usize, usize, usize),
) -> Option<u8> {
    let (ax, ay, w, h) = area;
    let steps = 32;
    let score = |turn: u8| -> usize {
        let mut matches = 0;
        for j in 0..steps {
            for i in 0..steps {
                let (x, y) = ((i * 2 + 1) * w / (steps * 2), (j * 2 + 1) * h / (steps * 2));
                let (tx, ty) = texel(turn, x, y, tsize);
                let t = &texture[ty * pitch + tx * 4..][..3];
                let d = &screen[(ay + y) * stride + (ax + x) * 4..][..3];
                let off: i32 = (0..3).map(|c| (t[c] as i32 - d[c] as i32).abs()).sum();
                if off < 24 {
                    matches += 1;
                }
            }
        }
        matches
    };
    let good = steps * steps * 85 / 100;
    let matching: Vec<u8> = (0..4u8)
        .filter(|&t| fits(t, tsize, (w, h)))
        .filter(|&t| score(t) >= good)
        .collect();
    match matching.as_slice() {
        [one] => Some(*one),
        _ => None,
    }
}

/// The picture of the whole desktop, kept up to date monitor by monitor.
pub struct Duplicator {
    outputs: Vec<Output>,
    origin: (i32, i32),
    size: (i32, i32),
    /// Blue, green, red and an unused byte for every pixel, top row first.
    pixels: Vec<u8>,
    /// What changed in the last [`Duplicator::refresh`], in the picture's
    /// coordinates.
    dirty: Vec<Area>,
}

impl Duplicator {
    /// Starts duplicating every monitor. `None` when this desktop is not the
    /// plain case, or duplication is not to be had (a remote session, an old
    /// driver).
    pub fn new() -> Option<Self> {
        let (origin, size) = desktop();
        if size.0 <= 0 || size.1 <= 0 {
            return None;
        }
        let mut outputs = Vec::new();
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1().ok()?;
            let mut adapter_index = 0;
            while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
                adapter_index += 1;
                // One device per graphics card, made when its first monitor
                // turns up: a card without monitors needs none.
                let mut card: Option<(ID3D11Device, ID3D11DeviceContext)> = None;
                let mut output_index = 0;
                while let Ok(output) = adapter.EnumOutputs(output_index) {
                    output_index += 1;
                    let about = output.GetDesc().ok()?;
                    if !about.AttachedToDesktop.as_bool() {
                        continue;
                    }
                    let upright = about.Rotation == DXGI_MODE_ROTATION_IDENTITY
                        || about.Rotation == DXGI_MODE_ROTATION_UNSPECIFIED;
                    // A monitor turned on its side hands its picture over
                    // unturned; it is turned when copied (see `find_turn`).
                    let area = about.DesktopCoordinates;
                    let (w, h) = (area.right - area.left, area.bottom - area.top);
                    let inside = area.left >= origin.0
                        && area.top >= origin.1
                        && area.right <= origin.0 + size.0
                        && area.bottom <= origin.1 + size.1;
                    if !inside || w <= 0 || h <= 0 {
                        return None;
                    }
                    if card.is_none() {
                        let (mut device, mut context) = (None, None);
                        D3D11CreateDevice(
                            &adapter,
                            D3D_DRIVER_TYPE_UNKNOWN,
                            HMODULE::default(),
                            D3D11_CREATE_DEVICE_FLAG(0),
                            None,
                            D3D11_SDK_VERSION,
                            Some(&mut device),
                            None,
                            Some(&mut context),
                        )
                        .ok()?;
                        card = Some((device?, context?));
                    }
                    let (device, context) = card.clone()?;
                    // The newer way first: on an HDR desktop it hands over an
                    // 8-bit picture, as asked; the older one refuses there.
                    let duplication = match output
                        .cast::<IDXGIOutput5>()
                        .and_then(|o| o.DuplicateOutput1(&device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM]))
                    {
                        Ok(d) => d,
                        Err(_) => output
                            .cast::<IDXGIOutput1>()
                            .ok()?
                            .DuplicateOutput(&device)
                            .ok()?,
                    };
                    let mode = duplication.GetDesc().ModeDesc;
                    let texture = (mode.Width as usize, mode.Height as usize);
                    let size = (w as usize, h as usize);
                    let shapes_fit = if upright {
                        fits(0, texture, size)
                    } else {
                        (0..4).any(|t| fits(t, texture, size))
                    };
                    if !shapes_fit || mode.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                        return None;
                    }
                    let readable = D3D11_TEXTURE2D_DESC {
                        Width: mode.Width,
                        Height: mode.Height,
                        MipLevels: 1,
                        ArraySize: 1,
                        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Usage: D3D11_USAGE_STAGING,
                        BindFlags: 0,
                        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                        MiscFlags: 0,
                    };
                    let mut staging = None;
                    device
                        .CreateTexture2D(&readable, None, Some(&mut staging))
                        .ok()?;
                    outputs.push(Output {
                        duplication,
                        context,
                        staging: staging?,
                        at: (
                            (area.left - origin.0) as usize,
                            (area.top - origin.1) as usize,
                        ),
                        size: (w as usize, h as usize),
                        seen: false,
                        drawn: false,
                        texture,
                        turn: upright.then_some(0),
                    });
                }
            }
            // Every monitor Windows knows must be among them, or part of the
            // picture would never arrive.
            if outputs.is_empty() || outputs.len() as i32 != GetSystemMetrics(SM_CMONITORS) {
                return None;
            }
        }
        Some(Self {
            outputs,
            origin,
            size,
            pixels: vec![0u8; size.0 as usize * size.1 as usize * 4],
            dirty: Vec::new(),
        })
    }

    /// Brings the picture up to date. `Ok(true)` when something on screen has
    /// changed since the last call, `Ok(false)` when nothing has; `Err` when
    /// duplication is lost (the desktop changed, a full-screen program took
    /// the monitor) and this duplicator is of no more use.
    pub fn refresh(&mut self) -> Result<bool, ()> {
        if desktop() != (self.origin, self.size) {
            return Err(());
        }
        self.dirty.clear();
        let mut changed = false;
        for i in 0..self.outputs.len() {
            let wait = if self.outputs[i].seen {
                0
            } else {
                FIRST_PICTURE_MS
            };
            changed |= self.take(i, wait)?;
        }
        Ok(changed)
    }

    /// Whether every monitor has handed over a picture that was really drawn.
    pub fn drawn(&self) -> bool {
        self.outputs.iter().all(|o| o.drawn)
    }

    /// Waits up to `ms` for every monitor to hand over a picture that was
    /// really drawn, for a capture made right after duplication was set up.
    /// `Ok(false)` when one did not come: the screen is not being drawn.
    pub fn wait_until_drawn(&mut self, ms: u64) -> Result<bool, ()> {
        let until = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while !self.drawn() {
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Ok(false);
            }
            for i in 0..self.outputs.len() {
                if !self.outputs[i].drawn {
                    self.take(i, (left.as_millis() as u32).clamp(1, 50))?;
                }
            }
        }
        Ok(true)
    }

    /// Takes monitor `i`'s next picture, waiting up to `wait` ms for it, and
    /// copies what changed into the whole picture. `Ok(true)` when something
    /// changed.
    fn take(&mut self, i: usize, wait: u32) -> Result<bool, ()> {
        let stride = self.size.0 as usize * 4;
        let mut changed = false;
        let output = &mut self.outputs[i];
        {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            unsafe {
                match output
                    .duplication
                    .AcquireNextFrame(wait, &mut info, &mut resource)
                {
                    Ok(()) => {}
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT && output.seen => {
                        return Ok(false);
                    }
                    Err(_) => return Err(()),
                }
                if info.LastPresentTime != 0 {
                    output.drawn = true;
                }
                // A present time of zero: only the mouse pointer moved, and
                // the pointer is not part of the picture.
                let copied = if info.LastPresentTime == 0 && output.seen {
                    Ok(false)
                } else {
                    let areas = if output.seen && output.turn == Some(0) {
                        output.changed(&info)
                    } else {
                        None
                    }
                    .unwrap_or_else(|| vec![Area::new(0, 0, output.size.0, output.size.1)]);
                    let copied = output.copy(resource, &mut self.pixels, stride, &areas);
                    self.dirty.extend(
                        areas
                            .iter()
                            .map(|a| Area::new(a.x + output.at.0, a.y + output.at.1, a.w, a.h)),
                    );
                    copied.map(|()| true)
                };
                let _ = output.duplication.ReleaseFrame();
                changed |= copied?;
            }
            output.seen = true;
        }
        Ok(changed)
    }

    /// What changed in the last [`Duplicator::refresh`].
    pub fn dirty(&self) -> &[Area] {
        &self.dirty
    }

    /// The desktop as of the last [`Duplicator::refresh`].
    pub fn raw(&self) -> Raw<'_> {
        Raw {
            bytes: &self.pixels,
            width: self.size.0 as u32,
            height: self.size.1 as u32,
            origin: self.origin,
        }
    }
}

impl Output {
    /// The parts of the monitor that changed in the frame just acquired:
    /// what was drawn, and where things were moved to. `None` when the
    /// system does not say.
    unsafe fn changed(&self, info: &DXGI_OUTDUPL_FRAME_INFO) -> Option<Vec<Area>> {
        let size = info.TotalMetadataBufferSize as usize;
        if size == 0 {
            return None;
        }
        let bounds = Area::new(0, 0, self.size.0, self.size.1);
        let area = |r: &RECT| {
            let (x0, y0) = (r.left.max(0) as usize, r.top.max(0) as usize);
            let (x1, y1) = (r.right.max(0) as usize, r.bottom.max(0) as usize);
            (x1 > x0 && y1 > y0)
                .then(|| Area::new(x0, y0, x1 - x0, y1 - y0))
                .and_then(|a| a.intersect(&bounds))
        };
        let mut out = Vec::new();
        unsafe {
            let count = size / size_of::<DXGI_OUTDUPL_MOVE_RECT>() + 1;
            let mut moves = vec![DXGI_OUTDUPL_MOVE_RECT::default(); count];
            let mut used = 0u32;
            self.duplication
                .GetFrameMoveRects(
                    (moves.len() * size_of::<DXGI_OUTDUPL_MOVE_RECT>()) as u32,
                    moves.as_mut_ptr(),
                    &mut used,
                )
                .ok()?;
            let n = used as usize / size_of::<DXGI_OUTDUPL_MOVE_RECT>();
            out.extend(moves[..n].iter().filter_map(|m| area(&m.DestinationRect)));
            let count = size / size_of::<RECT>() + 1;
            let mut rects = vec![RECT::default(); count];
            let mut used = 0u32;
            self.duplication
                .GetFrameDirtyRects(
                    (rects.len() * size_of::<RECT>()) as u32,
                    rects.as_mut_ptr(),
                    &mut used,
                )
                .ok()?;
            let n = used as usize / size_of::<RECT>();
            out.extend(rects[..n].iter().filter_map(area));
        }
        Some(out)
    }

    /// Copies the parts `areas` (in the monitor's coordinates) of the
    /// monitor's new picture into their place in `pixels`, the picture of the
    /// whole desktop, whose rows are `stride` bytes long. A monitor turned on
    /// its side is copied whole, turned; the first time, which way is found
    /// by comparing with what GDI reads, and `Err` when that cannot be told.
    unsafe fn copy(
        &mut self,
        resource: Option<IDXGIResource>,
        pixels: &mut [u8],
        stride: usize,
        areas: &[Area],
    ) -> Result<(), ()> {
        unsafe {
            let texture: ID3D11Texture2D = resource.ok_or(())?.cast().map_err(|_| ())?;
            self.context.CopyResource(&self.staging, &texture);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context
                .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|_| ())?;
            let (w, h) = self.size;
            let (tw, th) = self.texture;
            let pitch = mapped.RowPitch as usize;
            if mapped.pData.is_null() || pitch < tw * 4 {
                self.context.Unmap(&self.staging, 0);
                return Err(());
            }
            let source =
                std::slice::from_raw_parts(mapped.pData as *const u8, pitch * (th - 1) + tw * 4);
            let turn = match self.turn {
                Some(turn) => turn,
                None => {
                    let seen = Grabber::new().ok().and_then(|mut grabber| {
                        let raw = grabber.grab().ok()?;
                        let fits = self.at.0 + w <= raw.width as usize
                            && self.at.1 + h <= raw.height as usize;
                        fits.then(|| {
                            find_turn(
                                source,
                                pitch,
                                self.texture,
                                raw.bytes,
                                raw.width as usize * 4,
                                (self.at.0, self.at.1, w, h),
                            )
                        })?
                    });
                    match seen {
                        Some(turn) => {
                            self.turn = Some(turn);
                            turn
                        }
                        None => {
                            self.context.Unmap(&self.staging, 0);
                            return Err(());
                        }
                    }
                }
            };
            if turn == 0 {
                for a in areas {
                    for y in a.y..(a.y + a.h).min(h) {
                        let from = y * pitch + a.x * 4;
                        let to = (self.at.1 + y) * stride + (self.at.0 + a.x) * 4;
                        let len = a.w.min(w - a.x) * 4;
                        pixels[to..to + len].copy_from_slice(&source[from..from + len]);
                    }
                }
            } else {
                for y in 0..h {
                    let row = (self.at.1 + y) * stride + self.at.0 * 4;
                    for x in 0..w {
                        let (tx, ty) = texel(turn, x, y, self.texture);
                        let from = ty * pitch + tx * 4;
                        pixels[row + x * 4..row + x * 4 + 4]
                            .copy_from_slice(&source[from..from + 4]);
                    }
                }
            }
            self.context.Unmap(&self.staging, 0);
            Ok(())
        }
    }
}

/// For `bruma doctor`: every monitor as the graphics cards see it, and
/// whether its picture can be duplicated: where, how it is turned, whether
/// it shows HDR, at what scale.
pub fn describe() -> Vec<String> {
    use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
    use windows::Win32::Graphics::Dxgi::{IDXGIOutput5, IDXGIOutput6};
    use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
    let mut lines = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return vec!["graphics: DXGI could not be opened".into()];
        };
        let mut a = 0;
        while let Ok(adapter) = factory.EnumAdapters1(a) {
            a += 1;
            let name = adapter
                .GetDesc1()
                .map(|d| {
                    let n = d.Description.iter().position(|&c| c == 0).unwrap_or(128);
                    String::from_utf16_lossy(&d.Description[..n])
                })
                .unwrap_or_default();
            let mut o = 0;
            let mut device: Option<ID3D11Device> = None;
            while let Ok(output) = adapter.EnumOutputs(o) {
                o += 1;
                let Ok(about) = output.GetDesc() else {
                    continue;
                };
                if !about.AttachedToDesktop.as_bool() {
                    continue;
                }
                let r = about.DesktopCoordinates;
                let turned = match about.Rotation.0 {
                    2 => "turned 90",
                    3 => "turned 180",
                    4 => "turned 270",
                    _ => "upright",
                };
                let (hdr, bits) = match output.cast::<IDXGIOutput6>().and_then(|o| o.GetDesc1()) {
                    Ok(d) => (
                        d.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
                        d.BitsPerColor,
                    ),
                    Err(_) => (false, 0),
                };
                let (mut dx, mut dy) = (96u32, 96u32);
                let _ = GetDpiForMonitor(about.Monitor, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
                if device.is_none() {
                    let mut made = None;
                    let _ = D3D11CreateDevice(
                        &adapter,
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_FLAG(0),
                        None,
                        D3D11_SDK_VERSION,
                        Some(&mut made),
                        None,
                        None,
                    );
                    device = made;
                }
                let duplicated = match &device {
                    None => "no device".to_string(),
                    Some(device) => {
                        // One at a time: a monitor is duplicated once per
                        // process.
                        let say = |r: windows::core::Result<IDXGIOutputDuplication>| match r {
                            Ok(d) => format!("ok (format {})", d.GetDesc().ModeDesc.Format.0),
                            Err(e) => format!("{:#010x}", e.code().0),
                        };
                        let wide = say(output.cast::<IDXGIOutput5>().and_then(|o| {
                            o.DuplicateOutput1(device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])
                        }));
                        let plain = say(output
                            .cast::<IDXGIOutput1>()
                            .and_then(|o| o.DuplicateOutput(device)));
                        format!("duplication {plain}, duplication1 {wide}")
                    }
                };
                lines.push(format!(
                    "monitor {} on {name}: {}x{} at {},{}, {turned}, scale {}%, {}{}; {duplicated}",
                    lines.len() + 1,
                    r.right - r.left,
                    r.bottom - r.top,
                    r.left,
                    r.top,
                    dx * 100 / 96,
                    if hdr { "HDR" } else { "SDR" },
                    if bits > 0 {
                        format!(" {bits}-bit")
                    } else {
                        String::new()
                    },
                ));
            }
        }
    }
    if lines.is_empty() {
        lines.push("monitors: none found through DXGI".into());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A desktop picture with no symmetry: every pixel tells where it is.
    fn desktop(w: usize, h: usize) -> Vec<u8> {
        (0..h)
            .flat_map(|y| {
                (0..w).flat_map(move |x| {
                    [
                        (x * 7 % 256) as u8,
                        (y * 5 % 256) as u8,
                        ((x * y) % 251) as u8,
                        0,
                    ]
                })
            })
            .collect()
    }

    /// The picture a monitor turned `turn` quarter turns would hand over.
    fn handed_over(screen: &[u8], w: usize, h: usize, turn: u8) -> (Vec<u8>, (usize, usize)) {
        let size = if turn % 2 == 1 { (h, w) } else { (w, h) };
        let mut texture = vec![0u8; size.0 * size.1 * 4];
        for y in 0..h {
            for x in 0..w {
                let (tx, ty) = texel(turn, x, y, size);
                let to = (ty * size.0 + tx) * 4;
                texture[to..to + 4].copy_from_slice(&screen[(y * w + x) * 4..][..4]);
            }
        }
        (texture, size)
    }

    #[test]
    fn the_way_a_turned_monitor_hands_its_picture_over_is_found() {
        let (w, h) = (90, 160);
        let screen = desktop(w, h);
        for turn in 0..4u8 {
            let (texture, size) = handed_over(&screen, w, h, turn);
            let found = find_turn(&texture, size.0 * 4, size, &screen, w * 4, (0, 0, w, h));
            assert_eq!(found, Some(turn), "turn {turn}");
        }
    }

    #[test]
    fn a_screen_of_one_colour_says_nothing() {
        let (w, h) = (64, 64);
        let screen = vec![40u8; w * h * 4];
        let found = find_turn(&screen, w * 4, (w, h), &screen, w * 4, (0, 0, w, h));
        assert_eq!(found, None);
    }

    #[test]
    fn a_monitor_further_along_the_desktop_is_compared_where_it_is() {
        // Two monitors side by side; the second one is turned.
        let (w, h) = (60, 100);
        let wide = 200;
        let mut whole = vec![0u8; wide * h * 4];
        let second = desktop(w, h);
        for y in 0..h {
            whole[(y * wide + 120) * 4..][..w * 4].copy_from_slice(&second[y * w * 4..][..w * 4]);
        }
        let (texture, size) = handed_over(&second, w, h, 3);
        let found = find_turn(&texture, size.0 * 4, size, &whole, wide * 4, (120, 0, w, h));
        assert_eq!(found, Some(3));
    }
}
