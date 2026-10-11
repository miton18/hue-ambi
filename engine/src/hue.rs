//! Pont Hue : API REST v2 (HTTPS) et flux Entertainment (DTLS 1.2 PSK, UDP 2100).

use openssl::ssl::{Ssl, SslConnector, SslContext, SslMethod, SslStream, SslVerifyMode};
use serde_json::{json, Value};
use std::fmt::Display;
use std::io::{self, Read, Write};
use std::net::{TcpStream, UdpSocket};
use std::thread;
use std::time::Duration;

fn err<E: Display>(e: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, e.to_string())
}

/// Requete HTTPS minimale vers le pont. Certificat auto-signe : verification desactivee
/// (reseau local). HTTP/1.0 pour eviter le codage par blocs.
pub fn https(ip: &str, method: &str, path: &str, key: Option<&str>, body: Option<&str>) -> io::Result<(String, String)> {
    let mut b = SslConnector::builder(SslMethod::tls()).map_err(err)?;
    b.set_verify(SslVerifyMode::NONE);
    let conn = b.build();
    let tcp = TcpStream::connect((ip, 443))?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut cfg = conn.configure().map_err(err)?;
    cfg.set_verify_hostname(false);
    cfg.set_use_server_name_indication(false);
    let mut s = cfg.connect(ip, tcp).map_err(err)?;
    let body = body.unwrap_or("");
    let mut req = format!(
        "{method} {path} HTTP/1.0\r\nHost: {ip}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n",
        body.len()
    );
    if let Some(k) = key {
        req.push_str(&format!("hue-application-key: {k}\r\n"));
    }
    req.push_str("\r\n");
    req.push_str(body);
    s.write_all(req.as_bytes())?;
    let mut resp = Vec::new();
    let _ = s.read_to_end(&mut resp); // le pont ferme parfois sans close_notify
    let resp = String::from_utf8_lossy(&resp).into_owned();
    let (head, body) = resp.split_once("\r\n\r\n").unwrap_or((resp.as_str(), ""));
    Ok((head.to_string(), body.to_string()))
}

fn ok_status(head: &str) -> bool {
    head.lines().next().map_or(false, |l| l.split_whitespace().nth(1) == Some("200"))
}

/// Appairage : renvoie (cle d'application, clientkey). Exige un appui sur le bouton du pont.
pub fn pair(ip: &str) -> io::Result<(String, String)> {
    let body = json!({"devicetype": "hue-ambi#libreelec", "generateclientkey": true}).to_string();
    for _ in 0..30 {
        let (_, b) = https(ip, "POST", "/api", None, Some(&body))?;
        let v: Value = serde_json::from_str(&b).map_err(err)?;
        if let Some(s) = v.get(0).and_then(|x| x.get("success")) {
            let u = s["username"].as_str().ok_or_else(|| err("reponse sans username"))?;
            let c = s["clientkey"].as_str().ok_or_else(|| err("reponse sans clientkey"))?;
            return Ok((u.to_string(), c.to_string()));
        }
        thread::sleep(Duration::from_secs(2));
    }
    Err(err("bouton du pont non presse dans les 60 secondes"))
}

/// Identite PSK du flux Entertainment v2 : l'en-tete hue-application-id.
pub fn application_id(ip: &str, key: &str) -> io::Result<String> {
    let (head, _) = https(ip, "GET", "/auth/v1", Some(key), None)?;
    head.lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case("hue-application-id").then(|| v.trim().to_string())
        })
        .ok_or_else(|| err("en-tete hue-application-id absent (cle refusee ?)"))
}

pub fn areas(ip: &str, key: &str) -> io::Result<Value> {
    let (_, b) = https(ip, "GET", "/clip/v2/resource/entertainment_configuration", Some(key), None)?;
    serde_json::from_str(&b).map_err(err)
}

pub struct Channel {
    pub id: u8,
    pub x: f32,
    pub z: f32,
}

/// Canaux (lampes positionnees) d'une zone de divertissement.
pub fn channels(ip: &str, key: &str, area: &str) -> io::Result<Vec<Channel>> {
    let path = format!("/clip/v2/resource/entertainment_configuration/{area}");
    let (_, b) = https(ip, "GET", &path, Some(key), None)?;
    let v: Value = serde_json::from_str(&b).map_err(err)?;
    let chans = v["data"][0]["channels"].as_array().ok_or_else(|| err(format!("zone introuvable : {b}")))?;
    Ok(chans
        .iter()
        .filter_map(|c| {
            Some(Channel {
                id: c["channel_id"].as_u64()? as u8,
                x: c["position"]["x"].as_f64()? as f32,
                z: c["position"]["z"].as_f64()? as f32,
            })
        })
        .collect())
}

/// Demarre ou arrete le mode streaming de la zone.
pub fn set_streaming(ip: &str, key: &str, area: &str, on: bool) -> io::Result<()> {
    let body = json!({"action": if on { "start" } else { "stop" }}).to_string();
    let path = format!("/clip/v2/resource/entertainment_configuration/{area}");
    let (head, b) = https(ip, "PUT", &path, Some(key), Some(&body))?;
    if ok_status(&head) {
        Ok(())
    } else {
        Err(err(format!("{} {}", head.lines().next().unwrap_or(""), b)))
    }
}

fn hex(s: &str) -> io::Result<Vec<u8>> {
    let s = s.trim();
    if s.len() % 2 != 0 || s.is_empty() {
        return Err(err("clientkey hexadecimale invalide"));
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(err)).collect()
}

/// Socket UDP connectee : un write = un datagramme, un read = un datagramme.
#[derive(Debug)]
struct Udp(UdpSocket);

impl Read for Udp {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.recv(buf)
    }
}

impl Write for Udp {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.send(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Session de streaming Entertainment.
pub struct Stream {
    ssl: SslStream<Udp>,
    area: [u8; 36],
    seq: u8,
    buf: Vec<u8>,
}

impl Stream {
    pub fn connect(ip: &str, identity: &str, clientkey_hex: &str, area: &str) -> io::Result<Stream> {
        let psk = hex(clientkey_hex)?;
        let area_b: [u8; 36] = area.as_bytes().try_into().map_err(|_| err("identifiant de zone invalide"))?;
        let mut ctx = SslContext::builder(SslMethod::dtls()).map_err(err)?;
        ctx.set_cipher_list("PSK-AES128-GCM-SHA256").map_err(err)?;
        ctx.set_verify(SslVerifyMode::NONE);
        let ident = identity.to_owned();
        ctx.set_psk_client_callback(move |_ssl, _hint, id_out, psk_out| {
            let ib = ident.as_bytes();
            if ib.len() + 1 > id_out.len() || psk.len() > psk_out.len() {
                return Ok(0);
            }
            id_out[..ib.len()].copy_from_slice(ib);
            id_out[ib.len()] = 0;
            psk_out[..psk.len()].copy_from_slice(&psk);
            Ok(psk.len())
        });
        let ctx = ctx.build();
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        sock.connect((ip, 2100))?;
        sock.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut ssl = Ssl::new(&ctx).map_err(err)?;
        ssl.set_mtu(1400).map_err(err)?;
        let ssl = ssl.connect(Udp(sock)).map_err(|e| err(format!("poignee de main DTLS : {e}")))?;
        Ok(Stream { ssl, area: area_b, seq: 0, buf: Vec::with_capacity(52 + 7 * 20) })
    }

    /// Envoie une image : une couleur RVB 16 bits par canal.
    pub fn send(&mut self, chans: &[(u8, [u16; 3])]) -> io::Result<()> {
        packet(&mut self.buf, &self.area, self.seq, chans);
        self.seq = self.seq.wrapping_add(1);
        self.ssl.write(&self.buf).map(|_| ())
    }
}

/// Message HueStream v2.
fn packet(b: &mut Vec<u8>, area: &[u8; 36], seq: u8, chans: &[(u8, [u16; 3])]) {
    b.clear();
    b.extend_from_slice(b"HueStream");
    // version 2.0, sequence, 2 octets reserves, espace RVB, 1 octet reserve
    b.extend_from_slice(&[0x02, 0x00, seq, 0x00, 0x00, 0x00, 0x00]);
    b.extend_from_slice(area);
    for (id, c) in chans {
        b.push(*id);
        for v in c {
            b.extend_from_slice(&v.to_be_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn huestream_v2() {
        let area = *b"a1b2c3d4-0000-4000-8000-000000000001";
        let mut b = Vec::new();
        packet(&mut b, &area, 7, &[(0, [0xFFFF, 0x1234, 0]), (4, [1, 2, 3])]);
        assert_eq!(b.len(), 52 + 2 * 7);
        assert_eq!(&b[..9], b"HueStream");
        assert_eq!(&b[9..16], &[2, 0, 7, 0, 0, 0, 0]);
        assert_eq!(&b[16..52], &area);
        assert_eq!(&b[52..59], &[0, 0xFF, 0xFF, 0x12, 0x34, 0, 0]);
        assert_eq!(&b[59..66], &[4, 0, 1, 0, 2, 0, 3]);
    }

    #[test]
    fn clientkey_hex() {
        assert_eq!(hex("00ff10").unwrap(), vec![0, 255, 16]);
        assert!(hex("abc").is_err());
    }
}
