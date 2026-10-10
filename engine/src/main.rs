//! hue-ambi : Ambilight Philips Hue pour Raspberry Pi 5 / LibreELEC.
//!
//! Capture le plan video DRM, moyenne la zone de l'image qui fait face a chaque lampe
//! d'une zone de divertissement Hue, et pousse les couleurs en flux Entertainment (DTLS).

mod color;
mod drm;
mod hue;

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const CONF: &str = "/storage/.config/hue-ambi.conf";

/// Fichier de configuration : HUE_AMBI_CONF (fourni par l'add-on Kodi) ou chemin par defaut.
fn conf_path() -> String {
    std::env::var("HUE_AMBI_CONF").unwrap_or_else(|_| CONF.to_string())
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn load_conf(path: &str) -> io::Result<HashMap<String, String>> {
    let txt = std::fs::read_to_string(path)?;
    Ok(txt
        .lines()
        .filter_map(|l| {
            let l = l.trim();
            if l.starts_with('#') {
                return None;
            }
            let (k, v) = l.split_once('=')?;
            Some((k.trim().to_string(), v.trim().to_string()))
        })
        .collect())
}

struct Conf {
    bridge: String,
    key: String,
    clientkey: String,
    area: String,
    fps: f32,
    smoothing: f32,
    sync: drm::Sync,
    tuning: color::Tuning,
}

impl Conf {
    fn load() -> io::Result<Conf> {
        let path = conf_path();
        let m = load_conf(&path)?;
        let s = |k: &str| {
            m.get(k)
                .filter(|v| !v.is_empty())
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, format!("'{k}' manquant dans {path}")))
        };
        let f = |k: &str, d: f32| m.get(k).and_then(|v| v.parse().ok()).unwrap_or(d);
        Ok(Conf {
            bridge: s("bridge")?,
            key: s("key")?,
            clientkey: s("clientkey")?,
            area: s("area")?,
            fps: f("fps", 50.0).clamp(1.0, 60.0),
            smoothing: f("smoothing", 0.35).clamp(0.01, 1.0),
            sync: m.get("sync").and_then(|v| drm::Sync::parse(v)).unwrap_or(drm::Sync::Dc),
            tuning: color::Tuning { gain: f("gain", 1.0), saturation: f("saturation", 1.25), gamma: f("gamma", 2.2) },
        })
    }
}

/// Position d'une lampe dans la zone (x : gauche -1 .. droite +1, z : bas -1 .. haut +1)
/// -> region de l'image a moyenner, plaquee contre le bord le plus proche.
fn zones_for(chans: &[hue::Channel]) -> Vec<drm::Zone> {
    chans
        .iter()
        .map(|c| {
            let u = ((c.x + 1.0) / 2.0).clamp(0.0, 1.0);
            let v = ((1.0 - c.z) / 2.0).clamp(0.0, 1.0);
            drm::Zone { x0: (u - 0.2).max(0.0), x1: (u + 0.2).min(1.0), y0: (v - 0.25).max(0.0), y1: (v + 0.25).min(1.0) }
        })
        .collect()
}

fn sleep_s(s: u64) {
    for _ in 0..s * 4 {
        if STOP.load(Ordering::SeqCst) {
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
    }
}

fn run() -> io::Result<()> {
    let c = Conf::load()?;
    // un cran devant les taches ordinaires, sans passer devant Kodi en temps reel
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS as _, 0, -5);
    }
    let app_id = hue::application_id(&c.bridge, &c.key)?;
    let chans = hue::channels(&c.bridge, &c.key, &c.area)?;
    let zones = zones_for(&chans);
    eprintln!("hue-ambi : zone {} ({} canaux), {} img/s", c.area, chans.len(), c.fps);
    let mut cap = drm::Capture::new()?;
    cap.sync = c.sync;
    let period = Duration::from_secs_f32(1.0 / c.fps);
    let mut lin = vec![[0f32; 3]; zones.len()];
    let mut smooth = vec![[0f32; 3]; zones.len()];
    let mut out: Vec<(u8, [u16; 3])> = chans.iter().map(|ch| (ch.id, [0u16; 3])).collect();

    while !STOP.load(Ordering::SeqCst) {
        // --- attente d'une video a l'ecran ---
        if !cap.has_video() {
            std::thread::sleep(Duration::from_millis(250));
            continue;
        }
        eprintln!("hue-ambi : video detectee, ouverture du flux");
        if let Err(e) = hue::set_streaming(&c.bridge, &c.key, &c.area, true) {
            eprintln!("hue-ambi : demarrage de la zone : {e}");
            sleep_s(5);
            continue;
        }
        let mut stream = match hue::Stream::connect(&c.bridge, &app_id, &c.clientkey, &c.area) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("hue-ambi : {e}");
                let _ = hue::set_streaming(&c.bridge, &c.key, &c.area, false);
                sleep_s(5);
                continue;
            }
        };

        // --- flux ---
        let mut first = true;
        let mut misses = 0u32;
        let mut next = Instant::now();
        let (mut frames, mut busy, mut t_rep) = (0u64, Duration::ZERO, Instant::now());
        while !STOP.load(Ordering::SeqCst) {
            let t0 = Instant::now();
            if cap.grab(&zones, &mut lin) {
                misses = 0;
                let a = if first { 1.0 } else { c.smoothing };
                first = false;
                for (s, l) in smooth.iter_mut().zip(&lin) {
                    for k in 0..3 {
                        s[k] += a * (l[k] - s[k]);
                    }
                }
            } else {
                misses += 1;
                if misses as f32 > c.fps * 0.5 {
                    break; // plus de video depuis une demi-seconde
                }
            }
            for (o, s) in out.iter_mut().zip(&smooth) {
                o.1 = color::encode(*s, &c.tuning);
            }
            if let Err(e) = stream.send(&out) {
                eprintln!("hue-ambi : envoi : {e}");
                break;
            }
            busy += t0.elapsed();
            frames += 1;
            if t_rep.elapsed() >= Duration::from_secs(60) {
                let el = t_rep.elapsed().as_secs_f32();
                eprintln!(
                    "hue-ambi : {:.1} img/s, {:.2} ms de calcul par image",
                    frames as f32 / el,
                    busy.as_secs_f32() * 1000.0 / frames.max(1) as f32
                );
                frames = 0;
                busy = Duration::ZERO;
                t_rep = Instant::now();
            }
            next += period;
            let now = Instant::now();
            if next > now {
                std::thread::sleep(next - now);
            } else {
                next = now;
            }
        }
        drop(stream);
        cap.reset();
        let _ = hue::set_streaming(&c.bridge, &c.key, &c.area, false);
        eprintln!("hue-ambi : fin de video, flux ferme");
    }
    Ok(())
}

fn cmd_pair(ip: Option<&str>) -> io::Result<()> {
    let path = conf_path();
    let old = load_conf(&path).unwrap_or_default();
    let ip = ip
        .map(str::to_string)
        .or_else(|| old.get("bridge").cloned())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "usage : hue-ambi pair <ip_du_pont>"))?;
    println!("Appuie sur le bouton du pont Hue ({ip}) dans les 60 secondes...");
    let (key, clientkey) = hue::pair(&ip)?;
    let area = old.get("area").cloned().unwrap_or_default();
    let mut txt = format!("# hue-ambi\nbridge={ip}\nkey={key}\nclientkey={clientkey}\narea={area}\n");
    let mut rest: Vec<_> = old.iter().filter(|(k, _)| !["bridge", "key", "clientkey", "area"].contains(&k.as_str())).collect();
    rest.sort();
    for (k, v) in rest {
        txt.push_str(&format!("{k}={v}\n"));
    }
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
    std::io::Write::write_all(&mut f, txt.as_bytes())?;
    println!("Appairage reussi, identifiants ecrits dans {path}");
    if area.is_empty() {
        println!("Choisis une zone avec 'hue-ambi areas' puis renseigne area= dans {path}");
    }
    Ok(())
}

fn cmd_areas() -> io::Result<()> {
    let m = load_conf(&conf_path())?;
    let ip = m.get("bridge").cloned().unwrap_or_default();
    let key = m.get("key").cloned().unwrap_or_default();
    let v = hue::areas(&ip, &key)?;
    for a in v["data"].as_array().into_iter().flatten() {
        println!(
            "{}	{}	{}	{}",
            a["id"].as_str().unwrap_or("?"),
            a["metadata"]["name"].as_str().unwrap_or("?"),
            a["configuration_type"].as_str().unwrap_or("?"),
            a["channels"].as_array().map_or(0, |c| c.len())
        );
    }
    Ok(())
}

/// Diagnostic sans toucher aux lampes : identite DTLS, canaux, zones, et une capture.
fn cmd_test() -> io::Result<()> {
    let m = load_conf(&conf_path())?;
    let ip = m.get("bridge").cloned().unwrap_or_default();
    let key = m.get("key").cloned().unwrap_or_default();
    let area = m.get("area").cloned().unwrap_or_default();
    println!("identite DTLS : {}", hue::application_id(&ip, &key)?);
    println!("clientkey     : {}", if m.get("clientkey").map_or(false, |v| !v.is_empty()) { "presente" } else { "ABSENTE (lancer 'hue-ambi pair')" });
    let chans = hue::channels(&ip, &key, &area)?;
    let zones = zones_for(&chans);
    let tuning = color::Tuning { gain: 1.0, saturation: 1.25, gamma: 2.2 };
    let mut cap = drm::Capture::new()?;
    let mut lin = vec![[0f32; 3]; zones.len()];
    if !cap.grab(&zones, &mut lin) {
        for (ch, z) in chans.iter().zip(&zones) {
            println!("canal {}  pos=({:+.2},{:+.2})  zone x {:.2}-{:.2} y {:.2}-{:.2}", ch.id, ch.x, ch.z, z.x0, z.x1, z.y0, z.y1);
        }
        println!("aucune video a l'ecran : capture non testee");
        return Ok(());
    }
    for mode in [drm::Sync::None, drm::Sync::DmaBuf, drm::Sync::Dc] {
        cap.sync = mode;
        let n = 100;
        let t = Instant::now();
        for _ in 0..n {
            cap.grab(&zones, &mut lin);
        }
        let dt = t.elapsed() / n;
        println!("format {}  sync={:<6?} capture : {:.3} ms par image", String::from_utf8_lossy(&cap.last_format), mode, dt.as_secs_f64() * 1000.0);
    }
    for (ch, (l, z)) in chans.iter().zip(lin.iter().zip(&zones)) {
        let e = color::encode(*l, &tuning);
        println!(
            "canal {}  pos=({:+.2},{:+.2})  zone x {:.2}-{:.2} y {:.2}-{:.2}  rvb=({:3},{:3},{:3})",
            ch.id, ch.x, ch.z, z.x0, z.x1, z.y0, z.y1, e[0] >> 8, e[1] >> 8, e[2] >> 8
        );
    }
    Ok(())
}

/// Banc d'essai du flux DTLS contre un serveur PSK quelconque (port 2100) : 10 images rouges.
fn cmd_dtls(a: &[String]) -> io::Result<()> {
    let (ip, id, psk, area) = match a {
        [ip, id, psk, area, ..] => (ip, id, psk, area),
        _ => return Err(io::Error::new(io::ErrorKind::InvalidInput, "usage : hue-ambi dtls-test <ip> <identite> <psk_hex> <zone>")),
    };
    let t = Instant::now();
    let mut s = hue::Stream::connect(ip, id, psk, area)?;
    println!("poignee de main DTLS : {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    for _ in 0..10 {
        s.send(&[(0, [0xFFFF, 0, 0]), (1, [0, 0, 0xFFFF])])?;
        std::thread::sleep(Duration::from_millis(100));
    }
    println!("10 paquets envoyes");
    Ok(())
}

fn main() {
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    let args: Vec<String> = std::env::args().collect();
    let r = match args.get(1).map(String::as_str) {
        None | Some("run") => run(),
        Some("pair") => cmd_pair(args.get(2).map(String::as_str)),
        Some("areas") => cmd_areas(),
        Some("test") => cmd_test(),
        Some("dtls-test") => cmd_dtls(&args[2..]),
        Some(o) => {
            eprintln!("usage : hue-ambi [run | pair <ip_du_pont> | areas | test]   (commande inconnue : {o})");
            std::process::exit(2);
        }
    };
    if let Err(e) = r {
        eprintln!("hue-ambi : erreur : {e}");
        std::process::exit(1);
    }
}
