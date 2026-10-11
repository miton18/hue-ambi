//! Lecture directe du plan video DRM (Raspberry Pi 5, Kodi en "Direct To Plane").
//!
//! La video decodee est posee sur un plan materiel du controleur d'affichage et ne passe
//! jamais par OpenGL : on lit son framebuffer via les ioctl DRM (GETFB2 -> dma-buf -> mmap).
//!
//! Kodi cree un framebuffer par image affichee et le noyau recycle aussitot les numeros :
//! les tampons mappes sont donc indexes par l'inode de leur dma-buf, pas par numero de fb.
//!
//! Le numero du peripherique d'affichage depend de l'ordre de sondage des pilotes (sur Pi 5,
//! card0 est le GPU de rendu v3d et card1 le controleur d'affichage vc4) : on ouvre tous les
//! /dev/dri/cardN qui exposent des plans et on cherche la video sur chacun.

#[cfg(target_arch = "aarch64")]
use std::arch::asm;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::ptr;

use crate::color;

const DRI: &str = "/dev/dri";
const DEBUG_DRI: &str = "/sys/kernel/debug/dri";

/// Grille d'echantillonnage par zone.
const GX: usize = 12;
const GY: usize = 8;

const fn ioc(dir: u64, ty: u8, nr: u8, size: usize) -> u64 {
    (dir << 30) | ((size as u64) << 16) | ((ty as u64) << 8) | nr as u64
}
const RW: u64 = 3;
const W: u64 = 1;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct FbCmd2 {
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
const _: () = assert!(mem::size_of::<FbCmd2>() == 104);

#[repr(C)]
struct PrimeHandle {
    handle: u32,
    flags: u32,
    fd: i32,
}

#[repr(C)]
struct GemClose {
    handle: u32,
    pad: u32,
}

#[repr(C)]
#[derive(Default)]
struct GetPlane {
    plane_id: u32,
    crtc_id: u32,
    fb_id: u32,
    possible_crtcs: u32,
    gamma_size: u32,
    count_format_types: u32,
    format_type_ptr: u64,
}
const _: () = assert!(mem::size_of::<GetPlane>() == 32);

#[repr(C)]
#[derive(Default)]
struct GetPlaneRes {
    plane_id_ptr: u64,
    count_planes: u32,
    pad: u32,
}

#[repr(C)]
struct ClientCap {
    capability: u64,
    value: u64,
}

#[repr(C)]
struct DmaBufSync {
    flags: u64,
}

#[repr(C)]
#[derive(Default)]
struct Version {
    major: i32,
    minor: i32,
    patch: i32,
    name_len: usize,
    name: usize,
    date_len: usize,
    date: usize,
    desc_len: usize,
    desc: usize,
}

const VERSION: u64 = ioc(RW, b'd', 0x00, mem::size_of::<Version>());
const GETFB2: u64 = ioc(RW, b'd', 0xCE, mem::size_of::<FbCmd2>());
const PRIME_H2FD: u64 = ioc(RW, b'd', 0x2D, mem::size_of::<PrimeHandle>());
const GEM_CLOSE: u64 = ioc(W, b'd', 0x09, mem::size_of::<GemClose>());
const GETPLANERES: u64 = ioc(RW, b'd', 0xB5, mem::size_of::<GetPlaneRes>());
const GETPLANE: u64 = ioc(RW, b'd', 0xB6, mem::size_of::<GetPlane>());
const SET_CLIENT_CAP: u64 = ioc(W, b'd', 0x0D, mem::size_of::<ClientCap>());
const DMA_SYNC: u64 = ioc(W, b'b', 0x00, mem::size_of::<DmaBufSync>());
const UNIVERSAL_PLANES: u64 = 2;
const SYNC_START_READ: u64 = 1;
const SYNC_END_READ: u64 = 5;

fn ioctl<T>(fd: RawFd, req: u64, arg: &mut T) -> io::Result<()> {
    let r = unsafe { libc::ioctl(fd, req as libc::Ioctl, arg as *mut T) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}

fn is_yuv(f: u32) -> bool {
    [fourcc(b"P030"), fourcc(b"NV12"), fourcc(b"YU12"), fourcc(b"P010")].contains(&f)
}

/// Coherence entre le cache du CPU et la memoire ecrite par le decodeur.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sync {
    /// invalide uniquement les lignes de cache lues (`dc civac`), une seule barriere par image
    Dc,
    /// DMA_BUF_IOCTL_SYNC : synchronise tout le tampon (methode officielle, plus lente)
    DmaBuf,
    /// aucune (risque de couleurs figees si le tampon est mappe en cache)
    None,
}

impl Sync {
    pub fn parse(s: &str) -> Option<Sync> {
        match s {
            "dc" => Some(Sync::Dc),
            "dmabuf" => Some(Sync::DmaBuf),
            "none" => Some(Sync::None),
            _ => None,
        }
    }
}

/// Nettoie et invalide la ligne de cache contenant `p` (autorise en espace utilisateur sur arm64).
/// Sans barriere : l'appelant en place une seule apres toute la serie, avec `dsb()`.
#[inline(always)]
fn dc_civac(p: *const u8) {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        asm!("dc civac, {0}", in(reg) p, options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    let _ = p;
}

/// Attend la fin des operations de cache lancees avant de lire la memoire.
#[inline(always)]
fn dsb() {
    #[cfg(target_arch = "aarch64")]
    unsafe {
        asm!("dsb ish", options(nostack, preserves_flags));
    }
}

/// Nom du pilote DRM ("vc4", "v3d"...), pour le journal.
fn driver_name(fd: RawFd) -> String {
    let mut buf = [0u8; 32];
    let mut v = Version { name_len: buf.len(), name: buf.as_mut_ptr() as usize, ..Default::default() };
    if ioctl(fd, VERSION, &mut v).is_err() {
        return "?".into();
    }
    String::from_utf8_lossy(&buf[..v.name_len.min(buf.len())]).into_owned()
}

/// Zone de l'image a moyenner, en coordonnees normalisees 0..1.
#[derive(Clone, Copy, Debug)]
pub struct Zone {
    pub x0: f32,
    pub x1: f32,
    pub y0: f32,
    pub y1: f32,
}

fn after<'a>(s: &'a str, key: &str) -> Option<&'a str> {
    let i = s.find(key)? + key.len();
    Some(&s[i..])
}

/// "3840.000000x2076.000000+0.000000+0.000000" -> (x, y, w, h)
fn parse_src(s: &str) -> Option<(u32, u32, u32, u32)> {
    let s = s.split_whitespace().next()?;
    let (wh, xy) = s.split_once('+')?;
    let (w, h) = wh.split_once('x')?;
    let (x, y) = xy.split_once('+')?;
    let f = |v: &str| v.trim().parse::<f32>().ok().map(|f| f as u32);
    Some((f(x)?, f(y)?, f(w)?, f(h)?))
}

/// Rectangle source et HDR du plan, lus dans debugfs. debugfs verrouille tout l'affichage
/// pendant la lecture : on ne le consulte qu'au demarrage d'un flux ou a un changement de format.
fn plane_meta(minor: u32, id: u32) -> Option<((u32, u32, u32, u32), bool)> {
    let st = fs::read_to_string(format!("{DEBUG_DRI}/{minor}/state")).ok()?;
    let chunk = st
        .split("plane[")
        .skip(1)
        .find(|c| c.split(']').next().and_then(|v| v.trim().parse::<u32>().ok()) == Some(id))?;
    let chunk = chunk.split("crtc[").next().unwrap_or(chunk);
    let src = after(chunk, "src-pos=").and_then(parse_src).unwrap_or((0, 0, 0, 0));
    Some((src, chunk.contains("BT.2020")))
}

fn ensure_debugfs() {
    if fs::metadata(DEBUG_DRI).is_ok() {
        return;
    }
    let src = b"debugfs\0";
    let tgt = b"/sys/kernel/debug\0";
    unsafe {
        libc::mount(src.as_ptr() as *const _, tgt.as_ptr() as *const _, src.as_ptr() as *const _, 0, ptr::null());
    }
}

/// GETFB2, puis export du tampon en dma-buf si demande. Les handles GEM sont rendus aussitot.
fn export(card: RawFd, fb: u32, want_fd: bool) -> io::Result<(FbCmd2, Option<OwnedFd>)> {
    let mut c = FbCmd2 { fb_id: fb, ..Default::default() };
    ioctl(card, GETFB2, &mut c)?;
    let mut res = Ok(None);
    if want_fd {
        res = if c.handles[0] == 0 {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "GETFB2 sans handle"))
        } else {
            let mut p = PrimeHandle { handle: c.handles[0], flags: libc::O_CLOEXEC as u32, fd: -1 };
            ioctl(card, PRIME_H2FD, &mut p).map(|_| Some(unsafe { OwnedFd::from_raw_fd(p.fd) }))
        };
    }
    let mut done = [0u32; 4];
    for (i, &h) in c.handles.iter().enumerate() {
        if h != 0 && !done[..i].contains(&h) {
            let mut g = GemClose { handle: h, pad: 0 };
            let _ = ioctl(card, GEM_CLOSE, &mut g);
        }
        done[i] = h;
    }
    res.map(|fd| (c, fd))
}

fn inode(fd: &OwnedFd) -> io::Result<u64> {
    let mut st: libc::stat = unsafe { mem::zeroed() };
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(st.st_ino as u64)
}

fn same_layout(a: &FbCmd2, b: &FbCmd2) -> bool {
    (a.width, a.height, a.pixel_format, a.pitches, a.offsets, a.modifier[0])
        == (b.width, b.height, b.pixel_format, b.pitches, b.offsets, b.modifier[0])
}

/// Un tampon video, mappe en lecture seule.
struct Frame {
    fd: OwnedFd,
    ptr: *const u8,
    len: usize,
    cmd: FbCmd2,
    fourcc: [u8; 4],
    pitch: [usize; 4],
    off: [usize; 4],
    sand: bool,
    cs: usize,
    seen: u64,
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

impl Frame {
    fn map(fd: OwnedFd, c: &FbCmd2) -> io::Result<Frame> {
        let len = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
        if len <= 0 {
            return Err(io::Error::last_os_error());
        }
        let len = len as usize;
        let ptr = unsafe {
            libc::mmap(ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd.as_raw_fd(), 0)
        };
        if ptr == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let m = c.modifier[0];
        let sand = (m >> 56) == 0x07 && (m & 0xFF) == 4; // Broadcom SAND128
        let col_h = if sand { ((m & ((1u64 << 56) - 1)) >> 8) as usize } else { 0 };
        Ok(Frame {
            fd,
            ptr: ptr as *const u8,
            len,
            cmd: *c,
            fourcc: c.pixel_format.to_le_bytes(),
            pitch: c.pitches.map(|v| v as usize),
            off: c.offsets.map(|v| v as usize),
            sand,
            cs: 128 * col_h,
            seen: 0,
        })
    }

    fn sync(&self, flags: u64) {
        let mut s = DmaBufSync { flags };
        let _ = ioctl(self.fd.as_raw_fd(), DMA_SYNC, &mut s);
    }

    /// Lit `n` octets (1, 2 ou 4, jamais a cheval sur deux lignes de cache) en petit-boutiste.
    #[inline(always)]
    fn load(&self, a: usize, n: usize) -> Option<u32> {
        if a + n > self.len {
            return None;
        }
        let p = unsafe { self.ptr.add(a) };
        if n == 4 && a % 4 == 0 {
            return Some(u32::from_le(unsafe { ptr::read_volatile(p as *const u32) }));
        }
        let mut v = 0u32;
        for i in 0..n {
            v |= (unsafe { ptr::read_volatile(p.add(i)) } as u32) << (8 * i);
        }
        Some(v)
    }

    /// Invalide la ligne de cache de l'adresse `a` sans la lire (premiere passe de `grab`).
    #[inline(always)]
    fn flush(&self, a: usize, n: usize) -> Option<u32> {
        if a + n <= self.len {
            dc_civac(unsafe { self.ptr.add(a) });
        }
        Some(0)
    }

    /// (Y, Cb, Cr, bits) a la position (x, y). Chaque acces memoire passe par `rd(adresse, octets)` :
    /// la meme fonction sert a invalider le cache (`flush`) puis a lire (`load`), sans dupliquer
    /// le calcul des adresses.
    #[inline(always)]
    fn yuv_with(&self, x: usize, y: usize, rd: impl Fn(usize, usize) -> Option<u32>) -> Option<(u16, u16, u16, u8)> {
        let o = &self.off;
        let p = &self.pitch;
        let cs = self.cs;
        match (&self.fourcc, self.sand) {
            // P030 SAND128 : colonnes de 128 octets, 3 echantillons de 10 bits par mot de 32 bits
            (b"P030", true) => {
                let (col, xi) = (x / 96, x % 96);
                let k = xi / 2;
                let s = |base: usize, row: usize, i: usize| -> Option<u16> {
                    let v = rd(col * cs + base + row * 128 + (i / 3) * 4, 4)?;
                    Some(((v >> (10 * (i % 3))) & 0x3FF) as u16)
                };
                Some((s(o[0], y, xi)?, s(o[1], y / 2, 2 * k)?, s(o[1], y / 2, 2 * k + 1)?, 10))
            }
            (b"NV12", true) => {
                let (col, xi) = (x / 128, x % 128);
                let uv = rd(col * cs + o[1] + (y / 2) * 128 + (xi & !1), 2)?;
                let yy = rd(col * cs + o[0] + y * 128 + xi, 1)?;
                Some((yy as u16, (uv & 0xFF) as u16, (uv >> 8) as u16, 8))
            }
            (b"NV12", false) => {
                let uv = rd(o[1] + (y / 2) * p[1] + (x & !1), 2)?;
                let yy = rd(o[0] + y * p[0] + x, 1)?;
                Some((yy as u16, (uv & 0xFF) as u16, (uv >> 8) as u16, 8))
            }
            (b"YU12", false) => Some((
                rd(o[0] + y * p[0] + x, 1)? as u16,
                rd(o[1] + (y / 2) * p[1] + x / 2, 1)? as u16,
                rd(o[2] + (y / 2) * p[2] + x / 2, 1)? as u16,
                8,
            )),
            (b"P010", false) => {
                let yy = rd(o[0] + y * p[0] + x * 2, 2)?;
                let uv = rd(o[1] + (y / 2) * p[1] + (x / 2) * 4, 4)?;
                Some(((yy >> 6) as u16, ((uv & 0xFFFF) >> 6) as u16, (uv >> 22) as u16, 10))
            }
            _ => None,
        }
    }
}

/// Un peripherique DRM d'affichage (KMS), c'est-a-dire qui expose des plans.
struct Card {
    file: File,
    minor: u32,
    driver: String,
    planes: Vec<u32>,
}

impl Card {
    fn open(minor: u32) -> io::Result<Card> {
        let path = format!("{DRI}/card{minor}");
        let file = OpenOptions::new().read(true).write(true).custom_flags(libc::O_CLOEXEC).open(path)?;
        let fd = file.as_raw_fd();
        let mut cap = ClientCap { capability: UNIVERSAL_PLANES, value: 1 };
        let _ = ioctl(fd, SET_CLIENT_CAP, &mut cap);
        // echoue sur un GPU de rendu seul (v3d) : ce n'est pas un peripherique d'affichage
        let mut r = GetPlaneRes::default();
        ioctl(fd, GETPLANERES, &mut r)?;
        let mut planes = vec![0u32; r.count_planes as usize];
        r.plane_id_ptr = planes.as_mut_ptr() as u64;
        ioctl(fd, GETPLANERES, &mut r)?;
        planes.truncate(r.count_planes as usize);
        Ok(Card { minor, driver: driver_name(fd), planes, file })
    }

    fn fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }

    fn plane_fb(&self, id: u32) -> Option<u32> {
        let mut gp = GetPlane { plane_id: id, ..Default::default() };
        ioctl(self.fd(), GETPLANE, &mut gp).ok()?;
        (gp.fb_id != 0).then_some(gp.fb_id)
    }
}

/// Tous les /dev/dri/cardN qui exposent des plans, par numero croissant.
fn kms_cards() -> Vec<Card> {
    let mut minors: Vec<u32> = fs::read_dir(DRI)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().to_str()?.strip_prefix("card")?.parse().ok())
        .collect();
    minors.sort_unstable();
    minors.into_iter().filter_map(|m| Card::open(m).ok()).filter(|c| !c.planes.is_empty()).collect()
}

pub struct Capture {
    cards: Vec<Card>,
    /// tampons mappes, par inode de dma-buf
    cache: HashMap<u64, Frame>,
    /// (indice dans `cards`, plan) de la video en cours
    plane: Option<(usize, u32)>,
    meta: ((u32, u32, u32, u32), bool),
    meta_key: Option<(usize, u32, u32, u32, u32)>,
    tick: u64,
    pub sync: Sync,
    pub last_format: [u8; 4],
}

impl Capture {
    pub fn new() -> io::Result<Capture> {
        ensure_debugfs();
        let cards = kms_cards();
        if cards.is_empty() {
            return Err(io::Error::new(io::ErrorKind::NotFound, "aucun peripherique d'affichage DRM (KMS) dans /dev/dri"));
        }
        Ok(Capture {
            cards,
            cache: HashMap::new(),
            plane: None,
            meta: ((0, 0, 0, 0), false),
            meta_key: None,
            tick: 0,
            sync: Sync::Dc,
            last_format: *b"----",
        })
    }

    /// "/dev/dri/card1 (vc4, 13 plans)", pour le journal.
    pub fn describe(&self) -> String {
        let d: Vec<String> =
            self.cards.iter().map(|c| format!("{DRI}/card{} ({}, {} plans)", c.minor, c.driver, c.planes.len())).collect();
        d.join(", ")
    }

    /// Premier plan dont le framebuffer est en YUV, c'est-a-dire la video : (peripherique, plan, fb).
    fn find_video(&self) -> Option<(usize, u32, u32)> {
        self.cards.iter().enumerate().find_map(|(ci, card)| {
            card.planes.iter().find_map(|&id| {
                let fb = card.plane_fb(id)?;
                let (c, _) = export(card.fd(), fb, false).ok()?;
                is_yuv(c.pixel_format).then_some((ci, id, fb))
            })
        })
    }

    fn locate(&mut self) -> Option<(usize, u32, u32)> {
        if let Some((ci, id)) = self.plane {
            if let Some(fb) = self.cards[ci].plane_fb(id) {
                return Some((ci, id, fb));
            }
            self.plane = None;
        }
        let (ci, id, fb) = self.find_video()?;
        self.plane = Some((ci, id));
        Some((ci, id, fb))
    }

    /// Detection legere (quelques ioctl, sans debugfs) pour la boucle d'attente.
    pub fn has_video(&mut self) -> bool {
        self.plane = self.find_video().map(|(ci, id, _)| (ci, id));
        self.plane.is_some()
    }

    /// Fin de lecture : on rend tous les tampons a Kodi.
    pub fn reset(&mut self) {
        self.cache.clear();
        self.plane = None;
        self.meta_key = None;
    }

    /// Moyenne lineaire de chaque zone, ecrite dans `out`. Faux si aucune image exploitable.
    pub fn grab(&mut self, zones: &[Zone], out: &mut [[f32; 3]]) -> bool {
        let (ci, pid, fb) = match self.locate() {
            Some(v) => v,
            None => return false,
        };
        // le fb peut disparaitre entre GETPLANE et GETFB2 : image ratee, sans gravite
        let (c, fd) = match export(self.cards[ci].fd(), fb, true) {
            Ok((c, Some(fd))) => (c, fd),
            _ => return false,
        };
        if !is_yuv(c.pixel_format) {
            self.plane = None;
            return false;
        }
        let ino = match inode(&fd) {
            Ok(i) => i,
            Err(_) => return false,
        };
        self.tick += 1;
        let tick = self.tick;
        if self.cache.get(&ino).map_or(true, |f| !same_layout(&f.cmd, &c)) {
            match Frame::map(fd, &c) {
                Ok(f) => {
                    self.cache.insert(ino, f);
                }
                Err(_) => return false,
            }
        }
        // ne pas garder en vie (memoire CMA) les tampons que Kodi a liberes
        if tick % 64 == 0 {
            self.cache.retain(|_, f| tick - f.seen < 150);
        }
        let key = (ci, pid, c.width, c.height, c.pixel_format);
        if self.meta_key != Some(key) {
            let ten_bits = c.pixel_format == fourcc(b"P030") || c.pixel_format == fourcc(b"P010");
            self.meta = plane_meta(self.cards[ci].minor, pid).unwrap_or(((0, 0, 0, 0), ten_bits));
            self.meta_key = Some(key);
        }
        let ((sx, sy, mut sw, mut sh), hdr) = self.meta;
        let sync = self.sync;
        let frame = match self.cache.get_mut(&ino) {
            Some(f) => f,
            None => return false,
        };
        frame.seen = tick;
        let frame = &*frame;
        self.last_format = frame.fourcc;
        if sw == 0 || sh == 0 || sx + sw > c.width || sy + sh > c.height {
            (sw, sh) = (c.width, c.height);
        }
        let maxx = c.width.saturating_sub(1) as usize;
        let maxy = c.height.saturating_sub(1) as usize;
        // position dans le framebuffer du point (i, j) de la grille d'une zone
        let pos = |z: &Zone, i: usize, j: usize| -> (usize, usize) {
            let fx = z.x0 + (z.x1 - z.x0) * (i as f32 + 0.5) / GX as f32;
            let fy = z.y0 + (z.y1 - z.y0) * (j as f32 + 0.5) / GY as f32;
            (((sx as f32 + fx * sw as f32) as usize).min(maxx), ((sy as f32 + fy * sh as f32) as usize).min(maxy))
        };
        match sync {
            Sync::DmaBuf => frame.sync(SYNC_START_READ),
            // premiere passe : invalider toutes les lignes qui vont etre lues, puis une seule barriere
            Sync::Dc => {
                for z in zones {
                    for j in 0..GY {
                        for i in 0..GX {
                            let (x, y) = pos(z, i, j);
                            let _ = frame.yuv_with(x, y, |a, n| frame.flush(a, n));
                        }
                    }
                }
                dsb();
            }
            Sync::None => {}
        }
        let mut ok = true;
        for (z, o) in zones.iter().zip(out.iter_mut()) {
            let mut acc = [0f32; 3];
            let mut n = 0u32;
            for j in 0..GY {
                for i in 0..GX {
                    let (x, y) = pos(z, i, j);
                    if let Some((yy, cb, cr, bits)) = frame.yuv_with(x, y, |a, n| frame.load(a, n)) {
                        let c = color::to_linear(yy, cb, cr, bits, hdr);
                        acc[0] += c[0];
                        acc[1] += c[1];
                        acc[2] += c[2];
                        n += 1;
                    }
                }
            }
            if n == 0 {
                ok = false;
                continue;
            }
            let n = n as f32;
            *o = [acc[0] / n, acc[1] / n, acc[2] / n];
        }
        if sync == Sync::DmaBuf {
            frame.sync(SYNC_END_READ);
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn src_pos() {
        assert_eq!(parse_src("3840.000000x2076.000000+0.000000+42.000000 rot"), Some((0, 42, 3840, 2076)));
        assert_eq!(parse_src("n'importe quoi"), None);
    }

    #[test]
    fn formats() {
        assert!(is_yuv(fourcc(b"P030")) && is_yuv(fourcc(b"NV12")) && !is_yuv(fourcc(b"AR24")));
        assert_eq!(Sync::parse("dc"), Some(Sync::Dc));
        assert_eq!(GETFB2, 0xC068_64CE);
        assert_eq!(GETPLANE, 0xC020_64B6);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(VERSION, 0xC040_6400);
    }
}
