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
//!
//! Aucun code `unsafe` ici : les ioctl passent par les crates `drm` et `drm-ffi`, le mapping
//! et la coherence du cache CPU (DMA_BUF_IOCTL_SYNC) par la crate `dma-buf`.

use std::cell::Cell;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::num::NonZeroU32;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::MetadataExt;

use dma_buf::{DmaBuf, MappedDmaBuf};
use drm::buffer;
use drm::control::{plane, Device as ControlDevice};
use drm::{ClientCapability, Device};
use rustix::mount::{mount, MountFlags};

use crate::color;

const DRI: &str = "/dev/dri";
const DEBUG_DRI: &str = "/sys/kernel/debug/dri";

/// Grille d'echantillonnage par zone.
const GX: usize = 12;
const GY: usize = 8;

const fn fourcc(s: &[u8; 4]) -> u32 {
    u32::from_le_bytes(*s)
}

fn is_yuv(f: u32) -> bool {
    [fourcc(b"P030"), fourcc(b"NV12"), fourcc(b"YU12"), fourcc(b"P010")].contains(&f)
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
    let _ = mount("debugfs", "/sys/kernel/debug", "debugfs", MountFlags::empty(), None);
}

/// Geometrie d'un framebuffer, telle que rendue par GETFB2 (sans les handles GEM).
#[derive(Clone, Copy, PartialEq, Eq)]
struct Layout {
    width: u32,
    height: u32,
    pixel_format: u32,
    pitches: [u32; 4],
    offsets: [u32; 4],
    modifier: u64,
}

/// Un peripherique DRM d'affichage (KMS), c'est-a-dire qui expose des plans.
struct Card {
    file: File,
    minor: u32,
    driver: String,
    planes: Vec<plane::Handle>,
}

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl Device for Card {}
impl ControlDevice for Card {}

impl Card {
    fn open(minor: u32) -> io::Result<Card> {
        // la bibliotheque standard ouvre deja avec O_CLOEXEC
        let file = OpenOptions::new().read(true).write(true).open(format!("{DRI}/card{minor}"))?;
        let mut card = Card { file, minor, driver: String::new(), planes: Vec::new() };
        let _ = card.set_client_capability(ClientCapability::UniversalPlanes, true);
        // echoue sur un GPU de rendu seul (v3d) : ce n'est pas un peripherique d'affichage
        card.planes = card.plane_handles()?;
        card.driver = card.get_driver().map(|d| d.name().to_string_lossy().into_owned()).unwrap_or_else(|_| "?".into());
        Ok(card)
    }

    fn plane_fb(&self, plane: plane::Handle) -> Option<u32> {
        self.get_plane(plane).ok()?.framebuffer().map(u32::from)
    }

    /// GETFB2 brut : `drm-fourcc` ne connait pas P030 (HEVC 10 bits du Pi 5), d'ou `drm-ffi`
    /// plutot que `get_planar_framebuffer`. Les handles GEM rendus doivent etre refermes.
    fn layout(&self, fb: u32) -> io::Result<(Layout, [u32; 4])> {
        let c = drm_ffi::mode::get_framebuffer2(self.as_fd(), fb)?;
        let layout = Layout {
            width: c.width,
            height: c.height,
            pixel_format: c.pixel_format,
            pitches: c.pitches,
            offsets: c.offsets,
            modifier: c.modifier[0],
        };
        Ok((layout, c.handles))
    }

    /// Exporte le tampon du framebuffer en dma-buf, puis rend les handles GEM.
    fn export(&self, handles: [u32; 4]) -> io::Result<OwnedFd> {
        let fd = match NonZeroU32::new(handles[0]) {
            Some(h) => self.buffer_to_prime_fd(buffer::Handle::from(h), drm::CLOEXEC),
            None => Err(io::Error::new(io::ErrorKind::PermissionDenied, "GETFB2 sans handle")),
        };
        self.close_handles(handles);
        fd
    }

    fn close_handles(&self, handles: [u32; 4]) {
        for (i, &h) in handles.iter().enumerate() {
            if let Some(nz) = NonZeroU32::new(h) {
                if !handles[..i].contains(&h) {
                    let _ = self.close_buffer(buffer::Handle::from(nz));
                }
            }
        }
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

/// Un tampon video, mappe en lecture seule.
struct Frame {
    map: MappedDmaBuf,
    layout: Layout,
    fourcc: [u8; 4],
    pitch: [usize; 4],
    off: [usize; 4],
    sand: bool,
    cs: usize,
    seen: u64,
}

impl Frame {
    fn map(fd: OwnedFd, l: Layout) -> io::Result<Frame> {
        let map = DmaBuf::from(fd).memory_map()?;
        let m = l.modifier;
        let sand = (m >> 56) == 0x07 && (m & 0xFF) == 4; // Broadcom SAND128
        let col_h = if sand { ((m & ((1u64 << 56) - 1)) >> 8) as usize } else { 0 };
        Ok(Frame {
            map,
            layout: l,
            fourcc: l.pixel_format.to_le_bytes(),
            pitch: l.pitches.map(|v| v as usize),
            off: l.offsets.map(|v| v as usize),
            sand,
            cs: 128 * col_h,
            seen: 0,
        })
    }

    /// (Y, Cb, Cr, bits) a la position (x, y), lus dans `data` (le tampon mappe).
    #[inline(always)]
    fn yuv(&self, data: &[u8], x: usize, y: usize) -> Option<(u16, u16, u16, u8)> {
        // lecture petit-boutiste de 1, 2 ou 4 octets, bornee par la taille du tampon
        let rd = |a: usize, n: usize| -> Option<u32> {
            Some(match *data.get(a..a + n)? {
                [b0] => b0 as u32,
                [b0, b1] => u16::from_le_bytes([b0, b1]) as u32,
                [b0, b1, b2, b3] => u32::from_le_bytes([b0, b1, b2, b3]),
                _ => return None,
            })
        };
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

pub struct Capture {
    cards: Vec<Card>,
    /// tampons mappes, par inode de dma-buf
    cache: HashMap<u64, Frame>,
    /// (indice dans `cards`, plan) de la video en cours
    plane: Option<(usize, plane::Handle)>,
    meta: ((u32, u32, u32, u32), bool),
    meta_key: Option<(usize, plane::Handle, u32, u32, u32)>,
    tick: u64,
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
            last_format: *b"----",
        })
    }

    /// "/dev/dri/card1 (vc4, 56 plans)", pour le journal.
    pub fn describe(&self) -> String {
        let d: Vec<String> =
            self.cards.iter().map(|c| format!("{DRI}/card{} ({}, {} plans)", c.minor, c.driver, c.planes.len())).collect();
        d.join(", ")
    }

    /// Premier plan dont le framebuffer est en YUV, c'est-a-dire la video : (peripherique, plan, fb).
    fn find_video(&self) -> Option<(usize, plane::Handle, u32)> {
        self.cards.iter().enumerate().find_map(|(ci, card)| {
            card.planes.iter().find_map(|&plane| {
                let fb = card.plane_fb(plane)?;
                let (l, handles) = card.layout(fb).ok()?;
                card.close_handles(handles);
                is_yuv(l.pixel_format).then_some((ci, plane, fb))
            })
        })
    }

    fn locate(&mut self) -> Option<(usize, plane::Handle, u32)> {
        if let Some((ci, plane)) = self.plane {
            if let Some(fb) = self.cards[ci].plane_fb(plane) {
                return Some((ci, plane, fb));
            }
            self.plane = None;
        }
        let (ci, plane, fb) = self.find_video()?;
        self.plane = Some((ci, plane));
        Some((ci, plane, fb))
    }

    /// Detection legere (quelques ioctl, sans debugfs) pour la boucle d'attente.
    pub fn has_video(&mut self) -> bool {
        self.plane = self.find_video().map(|(ci, plane, _)| (ci, plane));
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
        let (ci, plane, fb) = match self.locate() {
            Some(v) => v,
            None => return false,
        };
        let card = &self.cards[ci];
        // le fb peut disparaitre entre GETPLANE et GETFB2 : image ratee, sans gravite
        let Ok((l, handles)) = card.layout(fb) else { return false };
        if !is_yuv(l.pixel_format) {
            card.close_handles(handles);
            self.plane = None;
            return false;
        }
        let Ok(fd) = card.export(handles) else { return false };
        // l'inode identifie le tampon reel, quel que soit le numero de fb que Kodi lui a donne
        let file = File::from(fd);
        let Ok(ino) = file.metadata().map(|m| m.ino()) else { return false };
        self.tick += 1;
        let tick = self.tick;
        if self.cache.get(&ino).is_none_or(|f| f.layout != l) {
            match Frame::map(OwnedFd::from(file), l) {
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
        let key = (ci, plane, l.width, l.height, l.pixel_format);
        if self.meta_key != Some(key) {
            let ten_bits = l.pixel_format == fourcc(b"P030") || l.pixel_format == fourcc(b"P010");
            self.meta = plane_meta(self.cards[ci].minor, u32::from(plane)).unwrap_or(((0, 0, 0, 0), ten_bits));
            self.meta_key = Some(key);
        }
        let ((sx, sy, mut sw, mut sh), hdr) = self.meta;
        let Some(frame) = self.cache.get_mut(&ino) else { return false };
        frame.seen = tick;
        let frame = &*frame;
        self.last_format = frame.fourcc;
        if sw == 0 || sh == 0 || sx + sw > l.width || sy + sh > l.height {
            (sw, sh) = (l.width, l.height);
        }
        let maxx = l.width.saturating_sub(1) as usize;
        let maxy = l.height.saturating_sub(1) as usize;
        // position dans le framebuffer du point (i, j) de la grille d'une zone
        let pos = |z: &Zone, i: usize, j: usize| -> (usize, usize) {
            let fx = z.x0 + (z.x1 - z.x0) * (i as f32 + 0.5) / GX as f32;
            let fy = z.y0 + (z.y1 - z.y0) * (j as f32 + 0.5) / GY as f32;
            (((sx as f32 + fx * sw as f32) as usize).min(maxx), ((sy as f32 + fy * sh as f32) as usize).min(maxy))
        };
        // `read` encadre la lecture par DMA_BUF_IOCTL_SYNC ; sa closure est `Fn`, d'ou les `Cell`
        // pour ecrire le resultat sans allocation
        let cells = Cell::from_mut(out).as_slice_of_cells();
        let res = frame.map.read(
            |data, _: Option<()>| {
                let mut ok = true;
                for (z, cell) in zones.iter().zip(cells) {
                    let mut acc = [0f32; 3];
                    let mut n = 0u32;
                    for j in 0..GY {
                        for i in 0..GX {
                            let (x, y) = pos(z, i, j);
                            if let Some((yy, cb, cr, bits)) = frame.yuv(data, x, y) {
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
                    cell.set([acc[0] / n, acc[1] / n, acc[2] / n]);
                }
                Ok(ok)
            },
            None,
        );
        res.unwrap_or(false)
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
    }
}
