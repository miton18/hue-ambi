//! Couleur : YCbCr brut -> RVB lineaire (1.0 = blanc de reference), puis encodage pour le flux Hue.

const M1: f32 = 0.159_301_76;
const M2: f32 = 78.843_75;
const C1: f32 = 0.835_937_5;
const C2: f32 = 18.851_562;
const C3: f32 = 18.687_5;

/// Blanc de reference d'un contenu HDR (ITU-R BT.2408) : 203 nits ~ blanc SDR.
const HDR_REF_WHITE: f32 = 203.0;

#[inline]
fn pq_to_nits(e: f32) -> f32 {
    if e <= 0.0 {
        return 0.0;
    }
    let p = e.min(1.0).powf(1.0 / M2);
    10_000.0 * ((p - C1).max(0.0) / (C2 - C3 * p)).powf(1.0 / M1)
}

#[inline]
fn gamma_to_lin(e: f32) -> f32 {
    e.clamp(0.0, 1.0).powf(2.4)
}

/// YCbCr (valeurs brutes, 8 ou 10 bits, plage limitee) -> RVB lineaire BT.709.
#[inline]
pub fn to_linear(y: u16, cb: u16, cr: u16, bits: u8, hdr: bool) -> [f32; 3] {
    let (yy, cb, cr) = if bits == 10 {
        ((y as f32 - 64.0) / 876.0, (cb as f32 - 512.0) / 896.0, (cr as f32 - 512.0) / 896.0)
    } else {
        ((y as f32 - 16.0) / 219.0, (cb as f32 - 128.0) / 224.0, (cr as f32 - 128.0) / 224.0)
    };
    if hdr {
        // BT.2020 non constant -> R'G'B' en PQ -> nits -> lineaire relatif au blanc de reference
        let r = pq_to_nits(yy + 1.4746 * cr) / HDR_REF_WHITE;
        let g = pq_to_nits(yy - 0.164_55 * cb - 0.571_35 * cr) / HDR_REF_WHITE;
        let b = pq_to_nits(yy + 1.8814 * cb) / HDR_REF_WHITE;
        // gamut BT.2020 -> BT.709 (lineaire)
        [
            (1.6605 * r - 0.5876 * g - 0.0728 * b).max(0.0),
            (-0.1246 * r + 1.1329 * g - 0.0083 * b).max(0.0),
            (-0.0182 * r - 0.1006 * g + 1.1187 * b).max(0.0),
        ]
    } else {
        [
            gamma_to_lin(yy + 1.5748 * cr),
            gamma_to_lin(yy - 0.1873 * cb - 0.4681 * cr),
            gamma_to_lin(yy + 1.8556 * cb),
        ]
    }
}

/// Reglages de rendu, lus dans le fichier de configuration.
pub struct Tuning {
    pub gain: f32,
    pub saturation: f32,
    pub gamma: f32,
}

/// Couleur lineaire (moyenne d'une zone, lissee) -> composantes 16 bits du flux Hue.
pub fn encode(c: [f32; 3], t: &Tuning) -> [u16; 3] {
    let l = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let mut out = [0u16; 3];
    for i in 0..3 {
        let v = ((l + (c[i] - l) * t.saturation) * t.gain).max(0.0);
        // genou doux : lineaire jusqu'a 0.7, compression asymptotique des hautes lumieres
        let v = if v <= 0.7 { v } else { 0.7 + 0.3 * (1.0 - (-(v - 0.7) / 0.3).exp()) };
        out[i] = (v.powf(1.0 / t.gamma).clamp(0.0, 1.0) * 65535.0) as u16;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn near(a: [f32; 3], b: [f32; 3], tol: f32) -> bool {
        a.iter().zip(&b).all(|(x, y)| (x - y).abs() <= tol)
    }

    #[test]
    fn sdr_blanc_noir_rouge() {
        assert!(near(to_linear(940, 512, 512, 10, false), [1.0; 3], 1e-3));
        assert!(near(to_linear(16, 128, 128, 8, false), [0.0; 3], 1e-3));
        // rouge pur BT.709, 8 bits plage limitee
        assert!(near(to_linear(63, 102, 240, 8, false), [1.0, 0.0, 0.0], 0.02));
    }

    #[test]
    fn hdr_blanc_de_reference() {
        // 203 nits = 58 % du signal PQ (BT.2408) -> Y' 10 bits = 64 + 0.58 * 876 = 572
        assert!(near(to_linear(572, 512, 512, 10, true), [1.0; 3], 0.03));
        assert!(near(to_linear(64, 512, 512, 10, true), [0.0; 3], 1e-4));
    }

    #[test]
    fn encodage() {
        let t = Tuning { gain: 1.0, saturation: 1.0, gamma: 2.2 };
        assert_eq!(encode([0.0; 3], &t), [0; 3]);
        let w = encode([1.0; 3], &t);
        assert!(w[0] == w[1] && w[1] == w[2] && w[0] > 60000);
        // les hautes lumieres HDR saturent en douceur au-dessus du blanc de reference
        let h = encode([50.0; 3], &t)[0];
        assert!(h > w[0]);
    }
}
