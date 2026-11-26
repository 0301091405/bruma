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
    IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CMONITORS};
use windows::core::Interface;

use super::windows::{Raw, desktop};

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
                    let area = about.DesktopCoordinates;
                    let (w, h) = (area.right - area.left, area.bottom - area.top);
                    let inside = area.left >= origin.0
                        && area.top >= origin.1
                        && area.right <= origin.0 + size.0
                        && area.bottom <= origin.1 + size.1;
                    if !upright || !inside || w <= 0 || h <= 0 {
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
                    let duplication = output
                        .cast::<IDXGIOutput1>()
                        .ok()?
                        .DuplicateOutput(&device)
                        .ok()?;
                    let mode = duplication.GetDesc().ModeDesc;
                    if (mode.Width as i32, mode.Height as i32) != (w, h)
                        || mode.Format != DXGI_FORMAT_B8G8R8A8_UNORM
                    {
                        return None;
                    }
                    let readable = D3D11_TEXTURE2D_DESC {
                        Width: w as u32,
                        Height: h as u32,
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
        let stride = self.size.0 as usize * 4;
        let mut changed = false;
        self.dirty.clear();
        for output in &mut self.outputs {
            let wait = if output.seen { 0 } else { FIRST_PICTURE_MS };
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            unsafe {
                match output
                    .duplication
                    .AcquireNextFrame(wait, &mut info, &mut resource)
                {
                    Ok(()) => {}
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT && output.seen => continue,
                    Err(_) => return Err(()),
                }
                // A present time of zero: only the mouse pointer moved, and
                // the pointer is not part of the picture.
                let copied = if info.LastPresentTime == 0 && output.seen {
                    Ok(false)
                } else {
                    let areas = if output.seen {
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
    /// whole desktop, whose rows are `stride` bytes long.
    unsafe fn copy(
        &self,
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
            let pitch = mapped.RowPitch as usize;
            if mapped.pData.is_null() || pitch < w * 4 {
                self.context.Unmap(&self.staging, 0);
                return Err(());
            }
            let source =
                std::slice::from_raw_parts(mapped.pData as *const u8, pitch * (h - 1) + w * 4);
            for a in areas {
                for y in a.y..(a.y + a.h).min(h) {
                    let from = y * pitch + a.x * 4;
                    let to = (self.at.1 + y) * stride + (self.at.0 + a.x) * 4;
                    let len = a.w.min(w - a.x) * 4;
                    pixels[to..to + len].copy_from_slice(&source[from..from + len]);
                }
            }
            self.context.Unmap(&self.staging, 0);
            Ok(())
        }
    }
}
