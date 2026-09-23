//! DRM dumb-buffer scanout for the phone panel. Same ioctl sequence the
//! panel has already been driven with: SET_MASTER, DSI connector, paint
//! before the first SETCRTC, then PAGE_FLIP.
#![allow(non_camel_case_types)]

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;

const DRM_IOCTL_BASE: u32 = b'd' as u32;
const _IOC_READ: u32 = 2;
const _IOC_WRITE: u32 = 1;
const fn iowr<T>(nr: u32) -> u32 {
    ((_IOC_READ | _IOC_WRITE) << 30)
        | ((std::mem::size_of::<T>() as u32) << 16)
        | (DRM_IOCTL_BASE << 8)
        | nr
}
const fn iow<T>(nr: u32) -> u32 {
    (_IOC_WRITE << 30)
        | ((std::mem::size_of::<T>() as u32) << 16)
        | (DRM_IOCTL_BASE << 8)
        | nr
}
const fn io(nr: u32) -> u32 {
    (DRM_IOCTL_BASE << 8) | nr
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_card_res {
    fb_id_ptr: u64,
    crtc_id_ptr: u64,
    connector_id_ptr: u64,
    encoder_id_ptr: u64,
    count_fbs: u32,
    count_crtcs: u32,
    count_connectors: u32,
    count_encoders: u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct drm_mode_modeinfo {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    r#type: u32,
    name: [u8; 32],
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_crtc {
    set_connectors_ptr: u64,
    count_connectors: u32,
    crtc_id: u32,
    fb_id: u32,
    x: u32,
    y: u32,
    gamma_size: u32,
    mode_valid: u32,
    mode: drm_mode_modeinfo,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_get_connector {
    encoders_ptr: u64,
    modes_ptr: u64,
    props_ptr: u64,
    prop_values_ptr: u64,
    count_modes: i32,
    count_props: i32,
    count_encoders: i32,
    encoder_id: u32,
    connector_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_get_encoder {
    encoder_id: u32,
    encoder_type: u32,
    crtc_id: u32,
    possible_crtcs: u32,
    possible_clones: u32,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_create_dumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
    size: u64,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_map_dumb {
    handle: u32,
    pad: u32,
    offset: u64,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_destroy_dumb {
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_fb_cmd2 {
    fb_id: u32,
    width: u32,
    height: u32,
    pixel_format: u32,
    flags: u32,
    handles: [u32; 4],
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: [u64; 4],
}

#[repr(C)]
#[derive(Default)]
struct drm_mode_crtc_page_flip {
    fb_id: u32,
    crtc_id: u32,
    flags: u32,
    reserved: u32,
    user_data: u64,
}

#[repr(C)]
#[derive(Default)]
struct drm_event_vblank {
    typ: u32,
    len: u32,
    user_data: u64,
    _tv_sec: u32,
    _tv_usec: u32,
    _sequence: u32,
    _crtc_id: u32,
}

const DRM_IOCTL_MODE_GETRESOURCES: u32 = iowr::<drm_mode_card_res>(0xA0);
const DRM_IOCTL_MODE_SETCRTC: u32 = iowr::<drm_mode_crtc>(0xA2);
const DRM_IOCTL_MODE_GETENCODER: u32 = iowr::<drm_mode_get_encoder>(0xA6);
const DRM_IOCTL_MODE_GETCONNECTOR: u32 = iowr::<drm_mode_get_connector>(0xA7);
const DRM_IOCTL_MODE_PAGE_FLIP: u32 = iowr::<drm_mode_crtc_page_flip>(0xB0);
const DRM_IOCTL_MODE_CREATE_DUMB: u32 = iowr::<drm_mode_create_dumb>(0xB2);
const DRM_IOCTL_MODE_MAP_DUMB: u32 = iowr::<drm_mode_map_dumb>(0xB3);
const DRM_IOCTL_MODE_ADDFB2: u32 = iowr::<drm_mode_fb_cmd2>(0xB8);
const DRM_IOCTL_MODE_RMFB: u32 = iow::<u32>(0xAF);
const DRM_IOCTL_MODE_DESTROY_DUMB: u32 = iowr::<drm_mode_destroy_dumb>(0xB4);
const DRM_IOCTL_SET_MASTER: u32 = io(0x1e);
const DRM_FORMAT_XRGB8888: u32 = 0x34325258;
const DRM_MODE_PAGE_FLIP_EVENT: u32 = 0x01;
const DRM_EVENT_VBLANK: u32 = 0x01;
const DRM_EVENT_FLIP_COMPLETE: u32 = 0x02;

pub struct Drm {
    file: File,
    pub width: u32,
    pub height: u32,
    pitch_px: usize,
    fb: [u32; 2],
    handles: [u32; 2],
    maps: [*mut u32; 2],
    map_len: usize,
    cur: usize,
    flip_ok: bool,
    crtc_id: u32,
    conn_id: u32,
    mode: drm_mode_modeinfo,
}

/// munmap → RMFB → DESTROY_DUMB. The order is load-bearing: the mapping
/// pins the buffer, the fb references the handle. Without munmap the vma's
/// vm_file keeps a struct file reference, drm_release never runs, and the
/// master dangles on the (long-lived) engine process — every later
/// SET_MASTER fails with nothing visible in /proc/*/fd (#85).
fn release_buffers(fd: i32, fb: &[u32; 2], handles: &[u32; 2], maps: &[*mut u32; 2], map_len: usize) {
    for m in maps.iter() {
        if !m.is_null() {
            unsafe { libc::munmap(*m as *mut libc::c_void, map_len) };
        }
    }
    for id in fb.iter() {
        if *id != 0 {
            let mut id = *id;
            unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_RMFB as _, &mut id) };
        }
    }
    for h in handles.iter() {
        if *h != 0 {
            let mut dd = drm_mode_destroy_dumb {
                handle: *h,
                ..Default::default()
            };
            unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_DESTROY_DUMB as _, &mut dd) };
        }
    }
}

impl Drm {
    /// One try. "master busy" means another process still owns the panel.
    pub fn open() -> Result<Drm, String> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_CLOEXEC)
            .open("/dev/dri/card0")
            .map_err(|e| format!("open card0: {e}"))?;
        let fd = file.as_raw_fd();
        if unsafe { libc::ioctl(fd, DRM_IOCTL_SET_MASTER as _) } != 0 {
            return Err("master busy".into());
        }
        let mut res = drm_mode_card_res::default();
        if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES as _, &mut res) } != 0 {
            return Err("GETRESOURCES #1 failed".into());
        }
        let mut crtcs = [0u32; 16];
        let mut conns = [0u32; 16];
        res.count_crtcs = res.count_crtcs.min(16);
        res.count_connectors = res.count_connectors.min(16);
        res.crtc_id_ptr = crtcs.as_mut_ptr() as u64;
        res.connector_id_ptr = conns.as_mut_ptr() as u64;
        res.count_fbs = 0;
        res.count_encoders = 0;
        if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETRESOURCES as _, &mut res) } != 0 {
            return Err("GETRESOURCES #2 failed".into());
        }
        let mut conn_id = 0u32;
        let mut enc_id = 0u32;
        let mut mode = drm_mode_modeinfo::default();
        for i in 0..res.count_connectors {
            let mut gc = drm_mode_get_connector::default();
            let mut modes = [drm_mode_modeinfo::default(); 8];
            let mut encs = [0u64; 8];
            gc.connector_id = conns[i as usize];
            gc.encoders_ptr = encs.as_mut_ptr() as u64;
            gc.count_encoders = 8;
            gc.modes_ptr = modes.as_mut_ptr() as u64;
            gc.count_modes = 8;
            if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETCONNECTOR as _, &mut gc) } != 0 {
                continue;
            }
            if gc.count_modes < 1 || gc.connector_type == 0 {
                continue;
            }
            let e = if gc.encoder_id != 0 {
                gc.encoder_id
            } else if gc.count_encoders > 0 {
                encs[0] as u32
            } else {
                0
            };
            if e == 0 {
                continue;
            }
            if conn_id == 0 || gc.connector_type == 16 {
                conn_id = conns[i as usize];
                enc_id = e;
                mode = modes[0];
                if gc.connector_type == 16 {
                    break;
                }
            }
        }
        if conn_id == 0 {
            return Err("no usable connector".into());
        }
        let mut crtc_id = 0u32;
        let mut ge = drm_mode_get_encoder::default();
        ge.encoder_id = enc_id;
        if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_GETENCODER as _, &mut ge) } == 0 {
            if ge.crtc_id != 0 {
                crtc_id = ge.crtc_id;
            } else {
                for c in 0..res.count_crtcs {
                    if ge.possible_crtcs & (1 << c) != 0 {
                        crtc_id = crtcs[c as usize];
                        break;
                    }
                }
            }
        }
        if crtc_id == 0 {
            return Err(format!("no crtc for enc {enc_id}"));
        }
        let mut fb = [0u32; 2];
        let mut maps = [std::ptr::null_mut::<u32>(); 2];
        let mut handles = [0u32; 2];
        let mut pitch_px = 0usize;
        let mut map_len = 0usize;
        for _b in 0..2 {
            let mut dumb = drm_mode_create_dumb {
                width: mode.hdisplay as u32,
                height: mode.vdisplay as u32,
                bpp: 32,
                ..Default::default()
            };
            if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB as _, &mut dumb) } != 0 {
                release_buffers(fd, &fb, &handles, &maps, map_len);
                return Err("CREATE_DUMB failed".into());
            }
            handles[_b] = dumb.handle;
            let mut fb2 = drm_mode_fb_cmd2::default();
            fb2.width = mode.hdisplay as u32;
            fb2.height = mode.vdisplay as u32;
            fb2.pixel_format = DRM_FORMAT_XRGB8888;
            fb2.handles[0] = dumb.handle;
            fb2.pitches[0] = dumb.pitch;
            if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_ADDFB2 as _, &mut fb2) } != 0 {
                release_buffers(fd, &fb, &handles, &maps, map_len);
                return Err("ADDFB2 failed".into());
            }
            let mut map = drm_mode_map_dumb {
                handle: dumb.handle,
                ..Default::default()
            };
            if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_MAP_DUMB as _, &mut map) } != 0 {
                release_buffers(fd, &fb, &handles, &maps, map_len);
                return Err("MAP_DUMB failed".into());
            }
            let m = unsafe {
                libc::mmap(
                    std::ptr::null_mut(),
                    dumb.size as usize,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    fd,
                    map.offset as libc::off_t,
                )
            };
            if m == libc::MAP_FAILED {
                release_buffers(fd, &fb, &handles, &maps, map_len);
                return Err("mmap dumb failed".into());
            }
            fb[_b] = fb2.fb_id;
            maps[_b] = m as *mut u32;
            pitch_px = dumb.pitch as usize / 4;
            map_len = dumb.size as usize;
        }
        Ok(Drm {
            file,
            width: mode.hdisplay as u32,
            height: mode.vdisplay as u32,
            pitch_px,
            fb,
            handles,
            maps,
            map_len,
            cur: 0,
            flip_ok: true,
            crtc_id,
            conn_id,
            mode,
        })
    }

    pub fn back_buf(&mut self) -> &mut [u32] {
        let p = self.maps[1 - self.cur];
        unsafe { std::slice::from_raw_parts_mut(p, self.pitch_px * self.height as usize) }
    }

    pub fn pitch_px(&self) -> usize {
        self.pitch_px
    }

    fn modeset(&self, fb_id: u32) -> Result<(), String> {
        let fd = self.file.as_raw_fd();
        let conn_list = [self.conn_id];
        let mut sc = drm_mode_crtc {
            set_connectors_ptr: conn_list.as_ptr() as u64,
            count_connectors: 1,
            crtc_id: self.crtc_id,
            fb_id,
            mode_valid: 1,
            mode: self.mode,
            ..Default::default()
        };
        let mut rc = unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_SETCRTC as _, &mut sc) };
        if rc != 0 {
            sc.set_connectors_ptr = 0;
            sc.count_connectors = 0;
            rc = unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_SETCRTC as _, &mut sc) };
        }
        if rc != 0 {
            return Err("SETCRTC failed".into());
        }
        Ok(())
    }

    /// Paint first, then this. The panel snapshots the buffer at modeset.
    pub fn initial_modeset(&mut self) -> Result<(), String> {
        let next = 1 - self.cur;
        self.modeset(self.fb[next])?;
        self.cur = next;
        Ok(())
    }

    fn wait_flip(&self, magic: u64) {
        let fd = self.file.as_raw_fd();
        let mut buf = [0u8; 1024];
        // One vblank. Waiting out a missed event (the old 8×50 ms) is
        // what made a finger feel stuck.
        for _ in 0..1 {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };
            if unsafe { libc::poll(&mut pfd, 1, 16) } <= 0 {
                return;
            }
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len()) };
            if n <= 0 {
                return;
            }
            let n = n as usize;
            let mut off = 0usize;
            while off + 8 <= n {
                let e = unsafe {
                    std::ptr::read_unaligned(buf.as_ptr().add(off) as *const drm_event_vblank)
                };
                let len = e.len as usize;
                if len < 8 || off + len > n {
                    return;
                }
                if (e.typ == DRM_EVENT_FLIP_COMPLETE || e.typ == DRM_EVENT_VBLANK)
                    && e.user_data == magic
                {
                    return;
                }
                off += len;
            }
        }
    }

    pub fn present(&mut self) {
        let next = 1 - self.cur;
        let fd = self.file.as_raw_fd();
        if self.flip_ok {
            const MAGIC: u64 = 0xA61B_7E5D_C0DE;
            let mut pf = drm_mode_crtc_page_flip {
                fb_id: self.fb[next],
                crtc_id: self.crtc_id,
                flags: DRM_MODE_PAGE_FLIP_EVENT,
                user_data: MAGIC,
                ..Default::default()
            };
            if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_PAGE_FLIP as _, &mut pf) } == 0 {
                self.wait_flip(MAGIC);
                self.cur = next;
                return;
            }
            self.flip_ok = false;
        }
        if self.modeset(self.fb[next]).is_ok() {
            self.cur = next;
        }
    }
}

impl Drop for Drm {
    fn drop(&mut self) {
        release_buffers(
            self.file.as_raw_fd(),
            &self.fb,
            &self.handles,
            &self.maps,
            self.map_len,
        );
        // The File's own Drop (close) lands after this; with the mappings
        // gone the last file reference falls, drm_release releases the
        // master, and the next SET_MASTER (term's re-grab) succeeds.
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The kernel dispatches ioctls on the full encoded command, so a wrong
    /// struct size turns RMFB/DESTROY_DUMB into a different ioctl entirely.
    /// These pin the encodings (and the 8-byte destroy_dumb) against Linux
    /// drm_ioctls.h (#85).
    #[test]
    fn drm_ioctl_numbers_match_kernel_encoding() {
        assert_eq!(std::mem::size_of::<drm_mode_destroy_dumb>(), 8);
        assert_eq!(DRM_IOCTL_MODE_RMFB, 0x4004_64af);
        assert_eq!(DRM_IOCTL_MODE_DESTROY_DUMB, 0xc008_64b4);
        // Existing encodings stay put alongside the new ones.
        assert_eq!(DRM_IOCTL_MODE_CREATE_DUMB, 0xc020_64b2);
        assert_eq!(DRM_IOCTL_MODE_PAGE_FLIP, 0xc018_64b0);
        assert_eq!(DRM_IOCTL_SET_MASTER, 0x641e);
    }
}
