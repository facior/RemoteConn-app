//! DXGI Desktop Duplication backend.

use std::time::Duration;

use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};

use crate::{cursor, CaptureError, CursorShape, CursorUpdate, Frame, MonitorInfo, Result};

impl From<windows::core::Error> for CaptureError {
    fn from(e: windows::core::Error) -> Self {
        CaptureError::Os(e.to_string())
    }
}

/// Visits every (adapter, output) pair in enumeration order.
fn for_each_output(
    mut f: impl FnMut(u32, &IDXGIAdapter1, IDXGIOutput1) -> Result<bool>,
) -> Result<()> {
    let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1()? };
    let mut id = 0;
    for a in 0.. {
        let adapter = match unsafe { factory.EnumAdapters1(a) } {
            Ok(adapter) => adapter,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(e.into()),
        };
        for o in 0.. {
            let output = match unsafe { adapter.EnumOutputs(o) } {
                Ok(output) => output,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(e.into()),
            };
            let desc = unsafe { output.GetDesc()? };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            if !f(id, &adapter, output.cast()?)? {
                return Ok(());
            }
            id += 1;
        }
    }
    Ok(())
}

pub fn monitors() -> Result<Vec<MonitorInfo>> {
    let mut out = Vec::new();
    for_each_output(|id, _, output| {
        let desc = unsafe { output.GetDesc()? };
        let r = desc.DesktopCoordinates;
        let name_len = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(32);
        out.push(MonitorInfo {
            id,
            name: String::from_utf16_lossy(&desc.DeviceName[..name_len]),
            x: r.left,
            y: r.top,
            width: (r.right - r.left) as u32,
            height: (r.bottom - r.top) as u32,
            primary: r.left == 0 && r.top == 0,
        });
        Ok(true)
    })?;
    Ok(out)
}

pub struct DxgiCapturer {
    monitor_id: u32,
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput1,
    duplication: Option<IDXGIOutputDuplication>,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
    buffer: Vec<u8>,
    width: u32,
    height: u32,
    have_frame: bool,
    /// Last reported pointer: visible, top-left of the image.
    pointer: Option<(bool, i32, i32)>,
    hotspot: (i32, i32),
    new_shape: Option<CursorShape>,
    /// What [`DxgiCapturer::take_cursor`] reported last.
    reported: Option<(bool, i32, i32)>,
    shape_buf: Vec<u8>,
}

// COM objects are only used from the thread that owns the capturer.
unsafe impl Send for DxgiCapturer {}

impl DxgiCapturer {
    pub fn new(monitor_id: u32) -> Result<Self> {
        let mut found = None;
        for_each_output(|id, adapter, output| {
            if id == monitor_id {
                found = Some((adapter.clone(), output));
                return Ok(false);
            }
            Ok(true)
        })?;
        let (adapter, output) = found.ok_or(CaptureError::MonitorNotFound(monitor_id))?;

        let mut device = None;
        let mut context = None;
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        Ok(Self {
            monitor_id,
            device: device.ok_or(CaptureError::Os("no D3D11 device".into()))?,
            context: context.ok_or(CaptureError::Os("no D3D11 context".into()))?,
            output,
            duplication: None,
            staging: None,
            buffer: Vec::new(),
            width: 0,
            height: 0,
            have_frame: false,
            pointer: None,
            hotspot: (0, 0),
            new_shape: None,
            reported: None,
            shape_buf: Vec::new(),
        })
    }

    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame<'_>>> {
        if self.duplication.is_none() {
            // The login screen / UAC prompt may have taken over: move to the
            // input desktop (works only as SYSTEM) and start afresh there.
            if crate::desktop::follow_input_desktop() {
                if let Ok(fresh) = Self::new(self.monitor_id) {
                    *self = fresh;
                }
            }
            match unsafe { self.output.DuplicateOutput(&self.device) } {
                Ok(dup) => {
                    self.duplication = Some(dup);
                    self.have_frame = false;
                }
                // Fails transiently on the secure desktop or while switching
                // modes; the caller just retries.
                Err(_) => {
                    std::thread::sleep(timeout);
                    return Ok(None);
                }
            }
        }
        let dup = self.duplication.clone().expect("duplication set above");

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        match unsafe { dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) } {
            Ok(()) => {}
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => {
                // Desktop switch, resolution change or fullscreen app.
                self.duplication = None;
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }

        self.read_pointer(&dup, &info);
        let copied = self.copy_to_staging(&info, resource);
        unsafe { dup.ReleaseFrame()? };
        if !copied? {
            return Ok(None);
        }
        self.read_staging()?;
        self.have_frame = true;
        Ok(Some(Frame {
            width: self.width,
            height: self.height,
            data: &self.buffer,
        }))
    }

    /// Records pointer movement and shape changes from the acquired frame.
    fn read_pointer(&mut self, dup: &IDXGIOutputDuplication, info: &DXGI_OUTDUPL_FRAME_INFO) {
        if info.LastMouseUpdateTime != 0 {
            let p = info.PointerPosition;
            self.pointer = Some((p.Visible.as_bool(), p.Position.x, p.Position.y));
        }
        let size = info.PointerShapeBufferSize;
        if size == 0 {
            return;
        }
        self.shape_buf.resize(size as usize, 0);
        let mut required = 0;
        let mut shape = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        let ok = unsafe {
            dup.GetFramePointerShape(
                size,
                self.shape_buf.as_mut_ptr().cast(),
                &mut required,
                &mut shape,
            )
        };
        if ok.is_ok() {
            self.hotspot = (shape.HotSpot.x, shape.HotSpot.y);
            self.new_shape = cursor::to_rgba(
                shape.Type,
                shape.Width,
                shape.Height,
                shape.Pitch,
                shape.HotSpot.x,
                shape.HotSpot.y,
                &self.shape_buf,
            );
        }
    }

    pub fn take_cursor(&mut self) -> Option<CursorUpdate> {
        let (visible, x, y) = self.pointer?;
        let at = (visible, x + self.hotspot.0, y + self.hotspot.1);
        if self.new_shape.is_none() && self.reported == Some(at) {
            return None;
        }
        self.reported = Some(at);
        Some(CursorUpdate {
            visible: at.0,
            x: at.1,
            y: at.2,
            shape: self.new_shape.take(),
        })
    }

    pub fn last_frame(&self) -> Option<Frame<'_>> {
        self.have_frame.then(|| Frame {
            width: self.width,
            height: self.height,
            data: &self.buffer,
        })
    }

    /// Returns false when only the mouse moved (no new desktop image).
    fn copy_to_staging(
        &mut self,
        info: &DXGI_OUTDUPL_FRAME_INFO,
        resource: Option<IDXGIResource>,
    ) -> Result<bool> {
        if info.LastPresentTime == 0 && self.have_frame {
            return Ok(false);
        }
        let texture: ID3D11Texture2D = resource
            .ok_or(CaptureError::Os("no desktop resource".into()))?
            .cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        unsafe { texture.GetDesc(&mut desc) };

        let reuse =
            matches!(&self.staging, Some((_, w, h)) if *w == desc.Width && *h == desc.Height);
        if !reuse {
            let staging_desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                MiscFlags: 0,
                MipLevels: 1,
                ArraySize: 1,
                ..desc
            };
            let mut staging = None;
            unsafe {
                self.device
                    .CreateTexture2D(&staging_desc, None, Some(&mut staging))?
            };
            let staging = staging.ok_or(CaptureError::Os("no staging texture".into()))?;
            self.staging = Some((staging, desc.Width, desc.Height));
            self.width = desc.Width;
            self.height = desc.Height;
        }
        let (staging, _, _) = self.staging.as_ref().expect("staging set above");
        unsafe { self.context.CopyResource(staging, &texture) };
        Ok(true)
    }

    fn read_staging(&mut self) -> Result<()> {
        let (staging, w, h) = self.staging.as_ref().expect("staging exists after copy");
        let (w, h) = (*w as usize, *h as usize);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?
        };
        let row = w * 4;
        self.buffer.resize(row * h, 0);
        let pitch = mapped.RowPitch as usize;
        // SAFETY: the mapped staging texture holds `h` rows of `pitch` bytes.
        let src = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, pitch * h) };
        for y in 0..h {
            self.buffer[y * row..(y + 1) * row].copy_from_slice(&src[y * pitch..y * pitch + row]);
        }
        unsafe { self.context.Unmap(staging, 0) };
        Ok(())
    }
}
